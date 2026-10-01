//! Start the run's MCP servers as children of crucible, one process per activated `[mcp]` server.
//!
//! The sandboxed agent has no direct authority to spend GPU, comment on JIRA, or roll a deployment;
//! it asks a server on the loop pod, which holds the privilege. crucible starts each server once per
//! run on its own port and blocks until the port accepts a connection, so the first turn's MCP calls
//! never race the boot. A catalog server gets only the env its catalog entry and the pack name, and
//! authenticates each sandbox by a token minted for that sandbox alone ([`tokens`]).
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
}

/// How long to wait for a server to start listening before giving up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a started server checks a sandbox's bearer against.
#[derive(Debug, Clone)]
pub enum ServerAuth {
    /// The desugared `[agent.broker]`: one token for every sandbox. `None` only when the broker was
    /// already listening and no `BROKER_TOKEN` is set.
    Shared(Option<String>),
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

/// Kills its child when the last handle drops: a server no handle reaches has no caller.
#[derive(Debug)]
struct ChildGuard(Mutex<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(child) = self.0.get_mut() {
            let _ = child.kill();
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
        let cmd = catalog_command(decl, &entry, &registry, ctx)?;
        planned.push((decl, entry.bin, cmd, registry));
    }
    for (decl, bin, cmd, registry) in planned {
        let child = spawn_and_wait(cmd, decl, &bin)?;
        runtime
            .children
            .push(Arc::new(ChildGuard(Mutex::new(child))));
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
        runtime
            .children
            .extend(child.map(|c| Arc::new(ChildGuard(Mutex::new(c)))));
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
/// `HOME`, the entry's required and optional names, the pack's env and secrets, the engine's
/// `MCP_*` names, and the output bounds.
fn catalog_command(
    decl: &McpServerDecl,
    entry: &catalog::McpCatalogEntry,
    registry: &tokens::TokenRegistry,
    ctx: &StartCtx<'_>,
) -> Result<Command> {
    use crucible_contract::mcp as wire;
    let lookup = |name: &str| {
        ctx.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let key = decl.key.to_string();
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

fn spawn_and_wait(mut cmd: Command, decl: &McpServerDecl, bin: &str) -> Result<Child> {
    let probe = format!("127.0.0.1:{}", decl.port);
    if port_open(&probe) {
        return Err(McpStartError::PortInUse {
            key: decl.key.to_string(),
            port: decl.port,
        }
        .into());
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning MCP server `{}` (`{bin}`)", decl.key))?;
    if wait_listening(&probe) {
        return Ok(child);
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(McpStartError::BootTimeout {
        key: decl.key.to_string(),
        bin: bin.to_string(),
        probe,
        seconds: BOOT_TIMEOUT.as_secs(),
    }
    .into())
}

/// Start the desugared `[agent.broker]` if it isn't already listening. `[agent].env` is
/// forwarded so the broker's backends see the same config the agent does; crucible's own
/// environment (KUBECONFIG, the PR token, ...) is inherited too, including `TRACEPARENT`,
/// deliberately: broker spans grafting onto the run trace is telemetry, not a leak.
///
/// Returns the bearer token guarding the broker and the child it started. `BROKER_TOKEN` in
/// crucible's env wins (an operator pairing with an externally-started broker); otherwise a fresh
/// random token is minted per run. No token only when the broker was already listening and no
/// `BROKER_TOKEN` is set: we can't retrofit a token onto a process we didn't start.
fn start_broker(cfg: &BrokerCfg, ctx: &StartCtx<'_>) -> Result<(Option<String>, Option<Child>)> {
    let env_token = std::env::var("BROKER_TOKEN").ok().filter(|t| !t.is_empty());
    let probe = probe_addr(&cfg.bind);
    if port_open(&probe) {
        return Ok((env_token, None));
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning the provisioning broker (`{}`)", cfg.bin))?;
    if wait_listening(&probe) {
        return Ok((Some(token), Some(child)));
    }
    let _ = child.kill();
    let _ = child.wait();
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
    def do_GET(self):
        bearer = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
        sandbox = None
        with open(os.environ["MCP_TOKENS_FILE"]) as f:
            for line in f:
                fields = line.split()
                if len(fields) == 2 and fields[0] == bearer:
                    sandbox = fields[1]
        body = json.dumps({"name": os.environ["MCP_NAME"], "sandbox": sandbox,
                           "env": sorted(os.environ)}).encode()
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
        let entry = |desc: &str, required: &str| {
            format!(
                "description = \"{desc}\"\nbin = \"python3\"\nargs = [\"{}\"]\nenv_required = [{required}]\nenv_optional = [\"ECHO_*\"]\n",
                script.display()
            )
        };
        std::fs::write(dir.join("echo-a.toml"), entry("a", "\"ECHO_URL\"")).unwrap();
        std::fs::write(dir.join("echo-b.toml"), entry("b", "")).unwrap();
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

        let alpha_a = registry("alpha").grant("ci-a").unwrap();
        let alpha_b = registry("alpha").grant("ci-b").unwrap();
        let beta_b = registry("beta").grant("ci-b").unwrap();

        let (status, alpha) = get(base, &alpha_a);
        assert_eq!(status, 200);
        assert_eq!(alpha["name"], "alpha");
        assert_eq!(alpha["sandbox"], "ci-a");
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
