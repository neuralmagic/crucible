//! Start the run's MCP servers as children of crucible, one process per activated `[mcp]` server.
//!
//! The sandboxed agent has no direct authority to spend GPU, comment on JIRA, or roll a deployment;
//! it asks a server on the loop pod, which holds the privilege. crucible starts each server once per
//! run on its own port and blocks until the port accepts a connection, so the first turn's MCP calls
//! never race the boot. A catalog server gets only the env its catalog entry names or offers the
//! pack, authenticates each sandbox by a token minted for that sandbox alone ([`tokens`]), and must
//! answer a request without one with 401 before the run uses it.
//!
//! A desugared `[agent.broker]` keeps its old contract for one release: crucible's whole
//! environment, one per-run `BROKER_TOKEN`, and the `BROKER_*` names.

pub(crate) mod catalog;
pub(crate) mod tokens;

use crate::manifest::{BrokerCfg, McpKey, McpServerDecl, McpSet, McpSource};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum McpStartError {
    #[error("MCP server `{key}` (`{bin}`) did not start listening on {probe} within {seconds}s")]
    BootTimeout {
        key: String,
        bin: String,
        probe: String,
        seconds: u64,
    },
    #[error("MCP server `{key}` needs {name}, which is not set on the loop pod")]
    MissingEnv { key: String, name: String },
    #[error("MCP server `{key}` holds secret {name}, which is not set on the loop pod")]
    MissingSecret { key: String, name: String },
    #[error("MCP server `{key}` cannot listen on port {port}: something already does")]
    PortInUse { key: String, port: u16 },
    #[error(
        "[mcp.{key}].env sets {name}, which catalog entry {catalog:?} does not list in pack_env"
    )]
    EnvNotOffered {
        key: String,
        catalog: String,
        name: String,
    },
    #[error(
        "MCP server `{key}` (`{bin}`) answered a request without a token with {answer}, not 401"
    )]
    Unauthenticated {
        key: String,
        bin: String,
        answer: String,
    },
    #[error(
        "something already listens on the [agent.broker] bind {bind} and BROKER_TOKEN is unset; \
         set BROKER_TOKEN to adopt it, or free the port"
    )]
    UnauthenticatedBroker { bind: String },
}

/// How long to wait for a server to start listening before giving up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a started server checks a sandbox's bearer against.
#[derive(Debug, Clone)]
pub enum ServerAuth {
    /// The desugared `[agent.broker]`: one token for every sandbox.
    Shared(String),
    /// One token per sandbox.
    PerSandbox(tokens::TokenRegistry),
}

/// One started server.
#[derive(Debug, Clone)]
pub struct RunningServer {
    pub key: McpKey,
    pub port: u16,
    /// An explicit `[agent.broker].url`, honored verbatim.
    pub url_override: Option<String>,
    pub auth: ServerAuth,
}

impl RunningServer {
    /// The URL a sandbox reaches the server at, `host` being the compute driver's name for the
    /// loop pod.
    pub fn url(&self, host: &str) -> String {
        match &self.url_override {
            Some(url) => url.clone(),
            None => format!("http://{host}:{}/mcp", self.port),
        }
    }
}

/// Kills its child's process group when the last handle drops: a server no handle reaches has no
/// caller, and a wrapper `bin` (`uvx`, a shell script) leaves the real server in that group.
#[derive(Debug)]
struct ChildGuard {
    child: Mutex<Child>,
    group: crucible::deadline::Group,
}

impl ChildGuard {
    /// Spawn `cmd` as the leader of a new process group.
    fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut child = cmd.process_group(0).spawn()?;
        match crucible::deadline::Group::track(&child) {
            Ok(group) => Ok(Self {
                child: Mutex::new(child),
                group,
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.group.kill();
        if let Ok(child) = self.child.get_mut() {
            let _ = child.wait();
        }
    }
}

/// The run's servers and the scope of the turn about to run.
#[derive(Debug, Clone, Default)]
pub struct McpRuntime {
    set: McpSet,
    servers: Vec<RunningServer>,
    scope: BTreeSet<McpKey>,
    children: Vec<Arc<ChildGuard>>,
    token_dir: Option<Arc<tempfile::TempDir>>,
}

impl McpRuntime {
    /// A runtime that starts nothing: a backend with no sandbox to reach a server from.
    pub fn idle(set: McpSet) -> Self {
        let scope = set.scope(None);
        Self {
            set,
            scope,
            ..Self::default()
        }
    }

    /// Narrow the turn to a named sandbox's servers.
    pub fn enter_sandbox(&mut self, name: &str) {
        self.scope = self.set.scope(Some(name));
    }

    /// The started servers the turn reaches, in key order.
    pub fn in_scope(&self) -> impl Iterator<Item = &RunningServer> {
        self.servers
            .iter()
            .filter(|server| self.scope.contains(&server.key))
    }

    /// Every started server.
    pub fn servers(&self) -> &[RunningServer] {
        &self.servers
    }

    #[cfg(test)]
    pub fn scope(&self) -> &BTreeSet<McpKey> {
        &self.scope
    }
}

/// What starting the servers needs from the run.
pub struct StartCtx<'a> {
    pub catalog_dir: &'a Path,
    /// The loop pod's environment, the only source of a catalog server's env and secrets.
    pub vars: &'a [(String, String)],
    /// The manifest's `[agent].env`, handed to a desugared broker as before.
    pub agent_env: &'a [(String, String)],
    /// The frozen manifest's output bounds, handed to every server after everything else.
    pub bounds_env: &'a [(String, String)],
    pub control_port: Option<u16>,
    /// The loop's sandbox, for a desugared broker's build sync.
    pub sandbox_name: &'a str,
    /// The address a catalog server binds, `0.0.0.0` so the sandbox reaches it.
    pub bind_host: &'a str,
}

impl StartCtx<'_> {
    fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Start every server in `set`. A missing catalog entry, an unset required env, or a missing
/// secret refuses the run before any catalog server starts.
pub fn start(set: McpSet, ctx: &StartCtx<'_>) -> Result<McpRuntime> {
    let mut runtime = McpRuntime::idle(set.clone());
    let mut planned = Vec::new();
    for decl in &set.servers {
        let McpSource::Catalog(name) = &decl.source else {
            continue;
        };
        let entry = catalog::load(ctx.catalog_dir, name)
            .with_context(|| format!("resolving [mcp.{}]", decl.key))?;
        let dir = match &runtime.token_dir {
            Some(dir) => dir.clone(),
            None => {
                let dir = tempfile::Builder::new()
                    .prefix("crucible-mcp-")
                    .tempdir()
                    .context("creating the MCP token directory")?;
                let dir = Arc::new(dir);
                runtime.token_dir = Some(dir.clone());
                dir
            }
        };
        let registry =
            tokens::TokenRegistry::create(dir.path().join(format!("{}.tokens", decl.key)))?;
        let cmd = catalog_command(decl, name, &entry, &registry, ctx)?;
        planned.push((decl, entry.bin, cmd, registry));
    }
    for (decl, bin, cmd, registry) in planned {
        let child = spawn_and_wait(cmd, decl, &bin)?;
        runtime.children.push(Arc::new(child));
        refuse_anonymous(decl, &bin)?;
        runtime.servers.push(RunningServer {
            key: decl.key.clone(),
            port: decl.port,
            url_override: None,
            auth: ServerAuth::PerSandbox(registry),
        });
    }
    for decl in &set.servers {
        let McpSource::Broker(cfg) = &decl.source else {
            continue;
        };
        tracing::warn!(
            "[agent.broker] is deprecated; declare it as [mcp.{}] and scope it with [agent].mcp \
             or a sandbox's mcp",
            decl.key
        );
        let (token, child) = start_broker(cfg, ctx)?;
        runtime.children.extend(child.map(Arc::new));
        runtime.servers.push(RunningServer {
            key: decl.key.clone(),
            port: decl.port,
            url_override: cfg.url.clone(),
            auth: ServerAuth::Shared(token),
        });
    }
    runtime.servers.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(runtime)
}

/// A catalog server's command: the entry's binary and args over an env holding only `PATH`,
/// `HOME`, the entry's required and optional names, the pack's env (only names the entry offers)
/// and secrets, the engine's `MCP_*` names, and the output bounds.
fn catalog_command(
    decl: &McpServerDecl,
    catalog: &str,
    entry: &catalog::McpCatalogEntry,
    registry: &tokens::TokenRegistry,
    ctx: &StartCtx<'_>,
) -> Result<Command> {
    use crucible_contract::mcp as wire;
    let lookup = |name: &str| ctx.var(name);
    let key = decl.key.to_string();
    if let Some(name) = decl.env.keys().find(|name| !entry.offers(name)) {
        return Err(McpStartError::EnvNotOffered {
            key,
            catalog: catalog.to_string(),
            name: name.clone(),
        }
        .into());
    }
    let mut cmd = Command::new(&entry.bin);
    cmd.args(&entry.args).env_clear();
    for name in ["PATH", "HOME"] {
        if let Some(value) = lookup(name) {
            cmd.env(name, value);
        }
    }
    for name in &entry.env_required {
        let value = lookup(name).ok_or_else(|| McpStartError::MissingEnv {
            key: key.clone(),
            name: name.clone(),
        })?;
        cmd.env(name, value);
    }
    for (name, value) in ctx.vars {
        if entry.passes_optional(name) {
            cmd.env(name, value);
        }
    }
    cmd.envs(&decl.env);
    for name in &decl.secrets {
        let value = lookup(name).ok_or_else(|| McpStartError::MissingSecret {
            key: key.clone(),
            name: name.clone(),
        })?;
        cmd.env(name, value);
    }
    cmd.env(wire::ENV_NAME, &key)
        .env(wire::ENV_BIND, format!("{}:{}", ctx.bind_host, decl.port))
        .env(wire::ENV_TOKENS_FILE, registry.path());
    if !decl.tools.is_empty() {
        cmd.env(wire::ENV_TOOLS, decl.tools.join(","));
    }
    if let Some(port) = ctx.control_port {
        cmd.env(wire::ENV_CONTROL_ADDR, format!("127.0.0.1:{port}"));
    }
    cmd.envs(ctx.bounds_env.iter().map(|(k, v)| (k, v)));
    Ok(cmd)
}

fn spawn_and_wait(mut cmd: Command, decl: &McpServerDecl, bin: &str) -> Result<ChildGuard> {
    let probe = format!("127.0.0.1:{}", decl.port);
    if port_open(&probe) {
        return Err(McpStartError::PortInUse {
            key: decl.key.to_string(),
            port: decl.port,
        }
        .into());
    }
    let child = ChildGuard::spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .with_context(|| format!("spawning MCP server `{}` (`{bin}`)", decl.key))?;
    if wait_listening(&probe) {
        return Ok(child);
    }
    drop(child);
    Err(McpStartError::BootTimeout {
        key: decl.key.to_string(),
        bin: bin.to_string(),
        probe,
        seconds: BOOT_TIMEOUT.as_secs(),
    }
    .into())
}

/// Refuse a server that serves a request carrying no bearer: it ignores `MCP_TOKENS_FILE`, and it
/// listens on every interface.
fn refuse_anonymous(decl: &McpServerDecl, bin: &str) -> Result<()> {
    let answer = match anonymous_status(decl.port) {
        Ok(401) => return Ok(()),
        Ok(status) => format!("HTTP {status}"),
        Err(e) => format!("no HTTP status ({e})"),
    };
    Err(McpStartError::Unauthenticated {
        key: decl.key.to_string(),
        bin: bin.to_string(),
        answer,
    }
    .into())
}

/// The status `POST /mcp` without `Authorization` gets on loopback `port`.
fn anonymous_status(port: u16) -> std::io::Result<u16> {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )?;
    stream.set_read_timeout(Some(BOOT_TIMEOUT))?;
    stream.set_write_timeout(Some(BOOT_TIMEOUT))?;
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head)?;
    std::str::from_utf8(&head)
        .ok()
        .filter(|line| line.starts_with("HTTP/1."))
        .and_then(|line| line.get(9..12))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other("not an HTTP status line"))
}

/// Start the desugared `[agent.broker]` if it isn't already listening. `[agent].env` is
/// forwarded so the broker's backends see the same config the agent does; crucible's own
/// environment (KUBECONFIG, the PR token, ...) is inherited too, including `TRACEPARENT`,
/// deliberately: broker spans grafting onto the run trace is telemetry, not a leak.
///
/// Returns the bearer token guarding the broker and the child it started. `BROKER_TOKEN` in
/// crucible's env wins (an operator pairing with an externally-started broker); otherwise a fresh
/// random token is minted per run. A listener already on the port is adopted only with
/// `BROKER_TOKEN` set: we can't retrofit a token onto a process we didn't start.
fn start_broker(cfg: &BrokerCfg, ctx: &StartCtx<'_>) -> Result<(String, Option<ChildGuard>)> {
    let env_token = ctx
        .var("BROKER_TOKEN")
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let probe = probe_addr(&cfg.bind);
    if port_open(&probe) {
        return match env_token {
            Some(token) => Ok((token, None)),
            None => Err(McpStartError::UnauthenticatedBroker {
                bind: cfg.bind.clone(),
            }
            .into()),
        };
    }
    let token = match env_token {
        Some(t) => t,
        None => tokens::mint_token().context("minting the broker bearer token")?,
    };

    let mut cmd = Command::new(&cfg.bin);
    cmd.env("BROKER_NAME", &cfg.name)
        .env("BROKER_BIND", &cfg.bind)
        .env("BROKER_TOKEN", &token)
        // The deep loop's per-workspace sandbox name (the pid in it is ours, crucible spawns the
        // broker), so the broker's build sync downloads from the right sandbox, not a fixed "ci".
        .env("BROKER_SANDBOX_NAME", ctx.sandbox_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep the broker's stderr (its one startup line + any error) in crucible's pod log.
        .stderr(Stdio::inherit());
    if let Some(port) = ctx.control_port {
        // The bridge binds 0.0.0.0:<port>; the broker is a same-pod child, so loopback reaches it.
        cmd.env("BROKER_CONTROL_ADDR", format!("127.0.0.1:{port}"));
    }
    if cfg.build {
        // Arm the engine-side build tools; the build target comes from the loop pod's
        // FORGE_* env, which the broker inherits along with the rest of crucible's environment.
        cmd.env("BROKER_BUILD", "1");
    }
    cmd.envs(ctx.agent_env.iter().map(|(k, v)| (k, v)));
    // The frozen manifest's output bounds, applied after `[agent].env` so a manifest key can
    // never shadow the bounds it is bounded by.
    cmd.envs(ctx.bounds_env.iter().map(|(k, v)| (k, v)));
    let child = ChildGuard::spawn(&mut cmd)
        .with_context(|| format!("spawning the provisioning broker (`{}`)", cfg.bin))?;
    if wait_listening(&probe) {
        return Ok((token, Some(child)));
    }
    drop(child);
    Err(McpStartError::BootTimeout {
        key: cfg.name.clone(),
        bin: cfg.bin.clone(),
        probe,
        seconds: BOOT_TIMEOUT.as_secs(),
    }
    .into())
}

fn wait_listening(probe: &str) -> bool {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    while Instant::now() < deadline {
        if port_open(probe) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// The address to probe for liveness. Binding `0.0.0.0` also listens on loopback, and connecting to
/// `0.0.0.0` isn't portable, so probe the bind port on `127.0.0.1`.
fn probe_addr(bind: &str) -> String {
    match bind.rsplit_once(':') {
        Some((_, port)) => format!("127.0.0.1:{port}"),
        None => bind.to_string(),
    }
}

/// True if a TCP connection to `addr` succeeds within a short timeout.
fn port_open(addr: &str) -> bool {
    addr.parse::<SocketAddr>()
        .ok()
        .and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).ok())
        .is_some()
}

#[cfg(test)]
mod tests {
    use crate::control::mcp::{
        McpRuntime, McpStartError, ServerAuth, StartCtx, port_open, probe_addr, start,
    };
    use crate::manifest::{Manifest, McpSet};
    use crucible_contract::mcp::TokenHolder;
    use std::path::Path;

    #[test]
    fn probe_addr_maps_bind_to_loopback_port() {
        assert_eq!(probe_addr("0.0.0.0:8849"), "127.0.0.1:8849");
        assert_eq!(probe_addr("0.0.0.0:1234"), "127.0.0.1:1234");
        assert_eq!(probe_addr("localhost"), "localhost");
    }

    #[test]
    fn port_open_false_on_garbage_and_dead_port() {
        assert!(!port_open("not-an-addr"));
        assert!(!port_open("127.0.0.1:1"));
    }

    /// A real HTTP server shaped like an MCP server's auth: answers every request with its name,
    /// the sandbox its bearer maps to in the token file, and its sorted env names.
    const ECHO_SERVER: &str = r#"
import http.server, json, os
host, port = os.environ["MCP_BIND"].rsplit(":", 1)
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.do_GET()
    def do_GET(self):
        bearer = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
        sandbox = workdir = None
        with open(os.environ["MCP_TOKENS_FILE"]) as f:
            for line in f:
                fields = line.split()
                if len(fields) in (2, 3) and fields[0] == bearer:
                    sandbox = fields[1]
                    workdir = fields[2] if len(fields) == 3 else None
        body = json.dumps({"name": os.environ["MCP_NAME"], "sandbox": sandbox,
                           "workdir": workdir, "env": sorted(os.environ)}).encode()
        self.send_response(200 if sandbox else 401)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer((host, int(port)), H).serve_forever()
"#;

    /// Two consecutive free loopback ports.
    fn free_port_pair() -> u16 {
        for _ in 0..50 {
            let Ok(first) = std::net::TcpListener::bind("127.0.0.1:0") else {
                continue;
            };
            let port = first.local_addr().unwrap().port();
            if port < u16::MAX && std::net::TcpListener::bind(("127.0.0.1", port + 1)).is_ok() {
                return port;
            }
        }
        panic!("no free port pair");
    }

    fn get(port: u16, bearer: &str) -> (u16, serde_json::Value) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET /mcp HTTP/1.0\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split_once("\r\n\r\n").unwrap().1;
        (status, serde_json::from_str(body).unwrap())
    }

    fn env_names(body: &serde_json::Value) -> Vec<String> {
        let names: Vec<String> = serde_json::from_value(body["env"].clone()).unwrap();
        names
            .into_iter()
            .filter(|k| !matches!(k.as_str(), "LC_CTYPE" | "__CF_USER_TEXT_ENCODING"))
            .collect()
    }

    fn set_for(manifest: &str, first_port: u16) -> McpSet {
        let m: Manifest = toml::from_str(manifest).unwrap();
        let scopes = crate::manifest::McpScopes {
            agent: &m.agent.mcp,
            sandboxes: m
                .agent
                .sandbox
                .iter()
                .map(|(name, p)| crate::manifest::SandboxScope {
                    name,
                    mcp: &p.mcp,
                    broker: p.broker,
                })
                .collect(),
        };
        McpSet::resolve_from(
            &m.mcp,
            &m.agent.broker,
            &scopes,
            &m.capabilities,
            first_port,
        )
        .unwrap()
    }

    fn write_catalog(dir: &Path, script: &Path) {
        let entry = |desc: &str, run: &str, required: &str| {
            format!(
                "description = \"{desc}\"\n{run}\nenv_required = [{required}]\nenv_optional = [\"ECHO_*\"]\npack_env = [\"PACK_SETTING\"]\n"
            )
        };
        let direct = format!("bin = \"python3\"\nargs = [\"{}\"]", script.display());
        let wrapped = format!(
            "bin = \"sh\"\nargs = [\"-c\", \"python3 {} ; exit 0\"]",
            script.display()
        );
        std::fs::write(dir.join("echo-a.toml"), entry("a", &direct, "\"ECHO_URL\"")).unwrap();
        std::fs::write(dir.join("echo-b.toml"), entry("b", &wrapped, "")).unwrap();
    }

    const MANIFEST: &str = r#"
        [repo]
        path = "."
        [agent]
        backend = "openshell"
        goal = "g"
        mcp = ["alpha"]
        [agent.sandbox.go]
        image = "img"
        mcp = ["alpha", "beta"]
        [[capabilities.secret]]
        name = "BETA_TOKEN"
        context = "broker"
        system = "beta"
        scope = "read"
        [mcp.alpha]
        catalog = "echo-a"
        env = { PACK_SETTING = "on" }
        tools = ["one", "two"]
        [mcp.beta]
        catalog = "echo-b"
        secrets = ["BETA_TOKEN"]
        [mcp.gamma]
        catalog = "echo-b"
    "#;

    #[test]
    fn each_server_starts_alone_with_a_minimal_env_and_per_sandbox_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(&script, ECHO_SERVER).unwrap();
        write_catalog(dir.path(), &script);
        let base = free_port_pair();
        let set = set_for(MANIFEST, base);

        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        vars.extend([
            ("ECHO_URL".to_string(), "http://jira".to_string()),
            ("ECHO_EXTRA".to_string(), "x".to_string()),
            ("BETA_TOKEN".to_string(), "s3cr3t".to_string()),
            ("UNRELATED_SECRET".to_string(), "leak".to_string()),
        ]);
        let ctx = StartCtx {
            catalog_dir: dir.path(),
            vars: &vars,
            agent_env: &[("AGENT_ONLY".to_string(), "x".to_string())],
            bounds_env: &[("BROKER_OUTPUTS".to_string(), "{}".to_string())],
            control_port: Some(4242),
            sandbox_name: "ci-unused",
            bind_host: "127.0.0.1",
        };
        let mut runtime = start(set, &ctx).expect("starts");
        let started: Vec<(&str, u16)> = runtime
            .servers()
            .iter()
            .map(|s| (s.key.as_str(), s.port))
            .collect();
        assert_eq!(
            started,
            [("alpha", base), ("beta", base + 1)],
            "gamma is unscoped, so it never starts"
        );
        let registry = |key: &str| {
            let server = runtime.servers().iter().find(|s| s.key.as_str() == key);
            match server.map(|s| &s.auth) {
                Some(ServerAuth::PerSandbox(r)) => r.clone(),
                _ => panic!("{key}: a catalog server has per-sandbox tokens"),
            }
        };

        let holder = |sandbox: &str| TokenHolder::new(sandbox, "/sandbox/workspace").unwrap();
        let alpha_a = registry("alpha").grant(&holder("ci-a")).unwrap();
        let alpha_b = registry("alpha").grant(&holder("ci-b")).unwrap();
        let beta_b = registry("beta").grant(&holder("ci-b")).unwrap();

        let (status, alpha) = get(base, &alpha_a);
        assert_eq!(status, 200);
        assert_eq!(alpha["name"], "alpha");
        assert_eq!(alpha["sandbox"], "ci-a");
        assert_eq!(alpha["workdir"], "/sandbox/workspace");
        assert_eq!(get(base, &alpha_b).1["sandbox"], "ci-b");
        assert_eq!(
            get(base, &beta_b).0,
            401,
            "alpha's file does not know beta's token"
        );
        assert_eq!(get(base + 1, &alpha_a).0, 401, "nor beta's alpha's");
        let (_, beta) = get(base + 1, &beta_b);
        assert_eq!(beta["sandbox"], "ci-b");

        let mut expected: Vec<String> = [
            "BROKER_OUTPUTS",
            "ECHO_EXTRA",
            "ECHO_URL",
            "MCP_BIND",
            "MCP_CONTROL_ADDR",
            "MCP_NAME",
            "MCP_TOKENS_FILE",
            "MCP_TOOLS",
            "PACK_SETTING",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for inherited in ["HOME", "PATH"] {
            if vars.iter().any(|(k, _)| k == inherited) {
                expected.push(inherited.to_string());
            }
        }
        expected.sort();
        assert_eq!(
            env_names(&alpha),
            expected,
            "only what the catalog and the pack name"
        );
        let beta_env = env_names(&beta);
        assert!(beta_env.contains(&"BETA_TOKEN".to_string()));
        assert!(
            !beta_env.contains(&"PACK_SETTING".to_string()),
            "alpha's pack env"
        );
        assert!(!beta_env.contains(&"MCP_TOOLS".to_string()), "no selection");

        let keys =
            |rt: &McpRuntime| -> Vec<String> { rt.in_scope().map(|s| s.key.to_string()).collect() };
        assert_eq!(keys(&runtime), ["alpha"]);
        runtime.enter_sandbox("go");
        assert_eq!(keys(&runtime), ["alpha", "beta"]);

        drop(runtime);
        assert!(
            !port_open(&format!("127.0.0.1:{base}")),
            "dropping the last handle stops the servers"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while port_open(&format!("127.0.0.1:{}", base + 1)) {
            assert!(
                std::time::Instant::now() < deadline,
                "beta's `sh` wrapper is gone, and so is the python3 it started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A plan fanning out to two isolated tasks: each task's sandbox gets its own token, and the
    /// server reads back the workdir that task's agent runs in, next to the loop turn's.
    #[test]
    fn an_isolated_fan_out_names_each_sandboxs_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(&script, ECHO_SERVER).unwrap();
        write_catalog(dir.path(), &script);
        let base = free_port_pair();
        let vars = vec![
            ("ECHO_URL".to_string(), "u".to_string()),
            ("BETA_TOKEN".to_string(), "s".to_string()),
        ];
        let runtime = start(set_for(MANIFEST, base), &bare_ctx(dir.path(), &vars)).unwrap();
        let Some(ServerAuth::PerSandbox(registry)) = runtime
            .servers()
            .iter()
            .find(|s| s.key.as_str() == "alpha")
            .map(|s| &s.auth)
        else {
            panic!("alpha has per-sandbox tokens");
        };

        let run = crate::args::Paths::for_manifest(
            dir.path().join("workspace"),
            dir.path().join("state"),
            dir.path(),
            None,
        );
        let tasks = [
            (
                "review/a",
                "/sandbox/task-26eb38badad9541f79ac17a40d91c9a0dbc62c0f117dbd7eae27d29f1fd5bc2c",
            ),
            (
                "review/b",
                "/sandbox/task-2d3a567e3f50d556ccb00be368bfd9437c497654cbca422a4fefec83272099ef",
            ),
        ];
        let mut turns = vec![(run.clone(), "/sandbox/workspace")];
        for (name, workdir) in tasks {
            let worktree = crate::plan::harness::task_worktree(&run, &name.into());
            turns.push((crate::args::Paths::for_worktree(worktree, None), workdir));
        }

        let mut granted = Vec::new();
        for (paths, workdir) in &turns {
            let holder = crate::openshell::run::token_holder(paths).unwrap();
            assert_eq!(holder.workdir(), Some(*workdir));
            let token = registry.grant(&holder).unwrap();
            granted.push((token, holder.sandbox().to_string(), *workdir));
        }
        let sandboxes: std::collections::BTreeSet<&str> =
            granted.iter().map(|(_, s, _)| s.as_str()).collect();
        assert_eq!(sandboxes.len(), 3, "each turn runs in its own sandbox");

        for (token, sandbox, workdir) in &granted {
            let (status, body) = get(base, token);
            assert_eq!(status, 200, "{sandbox}");
            assert_eq!(body["sandbox"], sandbox.as_str());
            assert_eq!(body["workdir"], *workdir);
        }

        let text = std::fs::read_to_string(registry.path()).unwrap();
        for (token, sandbox, workdir) in &granted {
            assert!(
                text.lines()
                    .any(|l| l == format!("{token} {sandbox} {workdir}")),
                "{text}"
            );
        }
    }

    fn bare_ctx<'a>(dir: &'a Path, vars: &'a [(String, String)]) -> StartCtx<'a> {
        StartCtx {
            catalog_dir: dir,
            vars,
            agent_env: &[],
            bounds_env: &[],
            control_port: None,
            sandbox_name: "ci",
            bind_host: "127.0.0.1",
        }
    }

    #[test]
    fn a_pack_env_name_the_entry_does_not_offer_refuses_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(&script, ECHO_SERVER).unwrap();
        write_catalog(dir.path(), &script);
        let base = free_port_pair();
        let vars = vec![
            ("ECHO_URL".to_string(), "u".to_string()),
            ("BETA_TOKEN".to_string(), "s".to_string()),
        ];
        for (name, value) in [
            ("LD_PRELOAD", "/opt/pack/x.so"),
            ("PATH", "/opt/pack/bin"),
            ("ECHO_URL", "https://attacker"),
        ] {
            let manifest = MANIFEST.replace(
                "env = { PACK_SETTING = \"on\" }",
                &format!("env = {{ {name} = \"{value}\" }}"),
            );
            let err = start(set_for(&manifest, base), &bare_ctx(dir.path(), &vars))
                .expect_err("not offered");
            assert!(
                matches!(
                    err.downcast_ref::<McpStartError>(),
                    Some(McpStartError::EnvNotOffered { key, name: got, .. })
                        if key == "alpha" && got == name
                ),
                "{name}: {err:#}"
            );
            assert!(!port_open(&format!("127.0.0.1:{base}")), "{name}");
        }
    }

    #[test]
    fn a_server_that_answers_without_a_token_is_refused_and_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("open.py");
        std::fs::write(
            &script,
            r#"
import http.server, os
host, port = os.environ["MCP_BIND"].rsplit(":", 1)
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()
    def log_message(self, *args):
        pass
http.server.HTTPServer((host, int(port)), H).serve_forever()
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("open.toml"),
            format!(
                "description = \"o\"\nbin = \"python3\"\nargs = [\"{}\"]\n",
                script.display()
            ),
        )
        .unwrap();
        let base = free_port_pair();
        let set = set_for(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "openshell"
            goal = "g"
            mcp = ["open"]
            [mcp.open]
            "#,
            base,
        );
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let err = start(set, &bare_ctx(dir.path(), &vars)).expect_err("serves anyone");
        assert!(
            matches!(
                err.downcast_ref::<McpStartError>(),
                Some(McpStartError::Unauthenticated { answer, .. }) if answer == "HTTP 200"
            ),
            "{err:#}"
        );
        assert!(
            !port_open(&format!("127.0.0.1:{base}")),
            "the refused server is stopped"
        );
    }

    #[test]
    fn a_legacy_broker_port_held_without_broker_token_is_refused() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        let set = set_for(
            &format!(
                r#"
                [repo]
                path = "."
                [agent]
                backend = "openshell"
                goal = "g"
                [agent.broker]
                enabled = true
                bin = "/nonexistent/broker"
                bind = "127.0.0.1:{port}"
                "#
            ),
            1,
        );
        let dir = tempfile::tempdir().unwrap();
        let err = start(set.clone(), &bare_ctx(dir.path(), &[])).expect_err("no token");
        assert!(
            matches!(
                err.downcast_ref::<McpStartError>(),
                Some(McpStartError::UnauthenticatedBroker { bind }) if *bind == format!("127.0.0.1:{port}")
            ),
            "{err:#}"
        );

        let vars = vec![("BROKER_TOKEN".to_string(), "operator".to_string())];
        let runtime = start(set, &bare_ctx(dir.path(), &vars)).expect("adopted with a token");
        assert!(matches!(
            &runtime.servers()[0].auth,
            ServerAuth::Shared(token) if token == "operator"
        ));
    }

    #[test]
    fn a_missing_entry_env_or_secret_refuses_before_anything_starts() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(&script, ECHO_SERVER).unwrap();
        let base = free_port_pair();
        let set = set_for(MANIFEST, base);
        let vars = vec![("BETA_TOKEN".to_string(), "s".to_string())];
        let ctx = StartCtx {
            catalog_dir: dir.path(),
            vars: &vars,
            agent_env: &[],
            bounds_env: &[],
            control_port: None,
            sandbox_name: "ci",
            bind_host: "127.0.0.1",
        };
        let err = start(set.clone(), &ctx).expect_err("no catalog");
        assert!(
            format!("{err:#}").contains("no entry \"echo-a\""),
            "{err:#}"
        );

        write_catalog(dir.path(), &script);
        let err = start(set.clone(), &ctx).expect_err("ECHO_URL unset");
        assert!(
            matches!(
                err.downcast_ref::<McpStartError>(),
                Some(McpStartError::MissingEnv { name, .. }) if name == "ECHO_URL"
            ),
            "{err:#}"
        );

        let vars = vec![("ECHO_URL".to_string(), "u".to_string())];
        let ctx = StartCtx { vars: &vars, ..ctx };
        let err = start(set, &ctx).expect_err("BETA_TOKEN unset");
        assert!(
            matches!(
                err.downcast_ref::<McpStartError>(),
                Some(McpStartError::MissingSecret { name, .. }) if name == "BETA_TOKEN"
            ),
            "{err:#}"
        );
        assert!(
            !port_open(&format!("127.0.0.1:{base}")),
            "alpha's checks passed, but nothing started"
        );
    }

    #[test]
    fn an_idle_runtime_tracks_scope_without_servers() {
        let set = set_for(MANIFEST, 1);
        let mut runtime = McpRuntime::idle(set);
        let scope: Vec<&str> = runtime.scope().iter().map(|k| k.as_str()).collect();
        assert_eq!(scope, ["alpha"]);
        runtime.enter_sandbox("go");
        assert_eq!(runtime.scope().len(), 2);
        assert_eq!(runtime.in_scope().count(), 0);
    }
}
