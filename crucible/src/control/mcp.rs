//! Start the pack's `[mcp]` servers as children of crucible, one process per server on its own
//! port, and hand each sandbox its own bearer token through the server's token file (see
//! [`crucible_contract::mcp`]).

use crate::control::broker::{mint_token, port_open};
use crate::manifest::McpCfg;
use anyhow::{Context, Result};
use crucible_contract::mcp::{self as wire, TokenHolder, TokenMap};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many of a server's last stderr lines a start failure carries.
const STDERR_TAIL: usize = 50;

/// How much of each of those lines it keeps.
const STDERR_LINE_CHARS: usize = 500;

/// How long an exited server's stderr gets to drain before the failure reports it.
const STDERR_DRAIN: Duration = Duration::from_secs(1);

/// A server that could not start. The run ends on it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StartError {
    #[error("[mcp.{key}] cannot listen on port {port}: something already does")]
    PortInUse { key: String, port: u16 },
    #[error("spawning [mcp.{key}] (`{bin}`)")]
    Spawn {
        key: String,
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("[mcp.{key}] (`{bin}`) exited ({status}) before listening on port {port}{stderr}")]
    Exited {
        key: String,
        bin: String,
        port: u16,
        status: ExitStatus,
        stderr: StderrShown,
    },
    #[error("[mcp.{key}] (`{bin}`) did not listen on port {port} within {BOOT_TIMEOUT:?}{stderr}")]
    BootTimeout {
        key: String,
        bin: String,
        port: u16,
        stderr: StderrShown,
    },
}

/// The stderr lines a start failure quotes.
#[derive(Debug)]
pub(crate) struct StderrShown(Vec<String>);

impl std::fmt::Display for StderrShown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return write!(f, "; it wrote nothing to stderr");
        }
        write!(f, "; its last {} stderr line(s):", self.0.len())?;
        for line in &self.0 {
            write!(f, "\n{line}")?;
        }
        Ok(())
    }
}

/// A server's last [`STDERR_TAIL`] stderr lines, redacted. Its raw stderr is copied to crucible's
/// stderr as it arrives.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn follow(stderr: Option<ChildStderr>) -> (Self, JoinHandle<()>) {
        let tail = Self::default();
        let lines = tail.clone();
        let reader = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Some(mut stderr) = stderr else { return };
            let mut chunk = [0u8; 8192];
            let mut line = Vec::new();
            let mut capped = false;
            while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                let _ = std::io::stderr().write_all(&chunk[..n]);
                for &byte in &chunk[..n] {
                    if byte == b'\n' {
                        if !capped {
                            lines.push(&line);
                        }
                        line.clear();
                        capped = false;
                    } else if !capped {
                        line.push(byte);
                        if line.len() == STDERR_LINE_CHARS * 4 {
                            lines.push(&line);
                            line.clear();
                            capped = true;
                        }
                    }
                }
            }
            if !line.is_empty() {
                lines.push(&line);
            }
        });
        (tail, reader)
    }

    fn push(&self, line: &[u8]) {
        let redacted = crate::turn_trace::redact(&String::from_utf8_lossy(line));
        let line = redacted.chars().take(STDERR_LINE_CHARS).collect();
        let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() == STDERR_TAIL {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn shown(&self) -> StderrShown {
        let lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        StderrShown(lines.iter().cloned().collect())
    }

    /// The tail once the server's stderr closes, or after [`STDERR_DRAIN`] when something else
    /// still holds it open.
    fn drained(&self, reader: &JoinHandle<()>) -> StderrShown {
        let until = Instant::now() + STDERR_DRAIN;
        while !reader.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.shown()
    }
}

/// One started server. Clones share the process, which is killed when the last one drops.
#[derive(Clone)]
pub(crate) struct Server {
    pub key: String,
    pub port: u16,
    pub tokens: TokenFile,
    _child: Arc<KillOnDrop>,
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start every server in `table`. A server gets only `PATH`, `HOME`, the names it inherits from
/// `vars` (the loop pod's env), its pack `env`, and the `MCP_*` names.
pub(crate) fn start(
    table: &BTreeMap<String, McpCfg>,
    vars: &[(String, String)],
) -> Result<Vec<Server>> {
    let mut servers = Vec::new();
    for ((key, cfg), port) in crate::manifest::mcp::ports(table) {
        let probe = format!("127.0.0.1:{port}");
        if port_open(&probe) {
            let key = key.clone();
            return Err(StartError::PortInUse { key, port }.into());
        }
        let tokens = TokenFile::create()?;
        let mut cmd = Command::new(&cfg.bin);
        cmd.args(&cfg.args).env_clear();
        for (name, value) in vars {
            if matches!(name.as_str(), "PATH" | "HOME") || cfg.inherit.contains(name) {
                cmd.env(name, value);
            }
        }
        cmd.envs(&cfg.env)
            .env(wire::ENV_NAME, key)
            .env(wire::ENV_BIND, format!("0.0.0.0:{port}"))
            .env(wire::ENV_TOKENS_FILE, tokens.path());
        if !cfg.tools.is_empty() {
            cmd.env(wire::ENV_TOOLS, cfg.tools.join(","));
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| StartError::Spawn {
                key: key.clone(),
                bin: cfg.bin.clone(),
                source,
            })?;
        let mut child = KillOnDrop(child);
        let (tail, reader) = StderrTail::follow(child.0.stderr.take());
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while !port_open(&probe) {
            if let Some(status) = child
                .0
                .try_wait()
                .with_context(|| format!("waiting on [mcp.{key}] (`{}`)", cfg.bin))?
            {
                return Err(StartError::Exited {
                    key: key.clone(),
                    bin: cfg.bin.clone(),
                    port,
                    status,
                    stderr: tail.drained(&reader),
                }
                .into());
            }
            if Instant::now() > deadline {
                return Err(StartError::BootTimeout {
                    key: key.clone(),
                    bin: cfg.bin.clone(),
                    port,
                    stderr: tail.shown(),
                }
                .into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        servers.push(Server {
            key: key.clone(),
            port,
            tokens,
            _child: Arc::new(child),
        });
    }
    Ok(servers)
}

/// One server's token file, rewritten by rename on every change.
#[derive(Clone)]
pub(crate) struct TokenFile {
    dir: Arc<tempfile::TempDir>,
    lock: Arc<Mutex<()>>,
}

impl TokenFile {
    fn create() -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("crucible-mcp-")
            .tempdir()
            .context("creating an MCP token directory")?;
        let file = Self {
            dir: Arc::new(dir),
            lock: Arc::default(),
        };
        file.write(&TokenMap::default())?;
        Ok(file)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("tokens")
    }

    /// Mint a token for `holder`'s sandbox, replacing any it held.
    pub fn grant(&self, holder: TokenHolder) -> Result<String> {
        let token = mint_token()?;
        self.update(|map| map.grant(holder, &token))?;
        Ok(token)
    }

    pub fn revoke(&self, sandbox: &str) -> Result<()> {
        self.update(|map| map.revoke(sandbox))
    }

    fn update(&self, change: impl FnOnce(&mut TokenMap)) -> Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let text = std::fs::read_to_string(self.path()).context("reading the MCP token file")?;
        let mut map = TokenMap::parse(&text)?;
        change(&mut map);
        self.write(&map)
    }

    fn write(&self, map: &TokenMap) -> Result<()> {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new_in(self.dir.path())?;
        tmp.write_all(map.render().as_bytes())?;
        tmp.persist(self.path())
            .context("replacing the MCP token file")?;
        Ok(())
    }
}

/// Grant `sandbox` a fresh token on each of `servers`, in order.
pub(crate) fn grant_all<'a>(
    servers: impl IntoIterator<Item = &'a Server>,
    sandbox: &str,
    workdir: &str,
) -> Result<Vec<String>> {
    servers
        .into_iter()
        .map(|s| s.tokens.grant(TokenHolder::new(sandbox, workdir)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::control::broker::port_open;
    use crate::control::mcp::{STDERR_LINE_CHARS, STDERR_TAIL, StartError, grant_all, start};
    use crate::manifest::McpCfg;
    use crucible_contract::mcp::TokenHolder;
    use std::collections::BTreeMap;

    /// Answers with its name, the holder its bearer maps to in the token file, and its env names.
    const ECHO_SERVER: &str = r#"
import http.server, json, os
host, port = os.environ["MCP_BIND"].rsplit(":", 1)
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        bearer = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
        holder = None
        with open(os.environ["MCP_TOKENS_FILE"]) as f:
            for line in f:
                fields = line.split()
                if fields and fields[0] == bearer:
                    holder = fields[1:]
        body = json.dumps({"name": os.environ["MCP_NAME"], "holder": holder,
                           "env": sorted(os.environ)}).encode()
        self.send_response(200 if holder else 401)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer((host, int(port)), H).serve_forever()
"#;

    fn get(port: u16, bearer: &str) -> (u16, serde_json::Value) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET /mcp HTTP/1.0\r\nAuthorization: Bearer {bearer}\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let body = response.split_once("\r\n\r\n").unwrap().1;
        (
            response[9..12].parse().unwrap(),
            serde_json::from_str(body).unwrap(),
        )
    }

    #[test]
    fn each_server_gets_a_minimal_env_and_maps_tokens_to_their_own_sandbox() {
        let _env = crucible::test_support::env_lock();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.py");
        std::fs::write(&script, ECHO_SERVER).unwrap();
        let cfg = |inherit: &[&str]| McpCfg {
            bin: "python3".into(),
            args: vec![script.display().to_string()],
            env: BTreeMap::from([("PACK_SETTING".into(), "on".into())]),
            inherit: inherit.iter().map(|s| s.to_string()).collect(),
            tools: vec!["build".into()],
        };
        let table = BTreeMap::from([
            ("alpha".into(), cfg(&["KUBE_HOST"])),
            ("beta".into(), cfg(&[])),
        ]);
        let vars: Vec<(String, String)> = std::env::vars()
            .chain([
                ("KUBE_HOST".into(), "10.0.0.1".into()),
                ("LOOP_SECRET".into(), "leak".into()),
            ])
            .collect();
        let servers = start(&table, &vars).expect("starts");
        let [alpha, beta] = &servers[..] else {
            panic!("two servers")
        };

        let a = alpha
            .tokens
            .grant(TokenHolder::new("ci-a", "/sandbox/workspace").unwrap())
            .unwrap();
        let b = alpha
            .tokens
            .grant(TokenHolder::new("ci-b", "/sandbox/task-1").unwrap())
            .unwrap();
        let (status, body) = get(alpha.port, &a);
        assert_eq!(status, 200);
        assert_eq!(body["name"], "alpha");
        assert_eq!(
            body["holder"],
            serde_json::json!(["ci-a", "/sandbox/workspace"])
        );
        assert_eq!(get(alpha.port, &b).1["holder"][0], "ci-b");
        assert_eq!(
            get(beta.port, &a).0,
            401,
            "beta's file does not know alpha's tokens"
        );

        let env: Vec<String> = serde_json::from_value(body["env"].clone()).unwrap();
        let mut expected = vec![
            "KUBE_HOST",
            "MCP_BIND",
            "MCP_NAME",
            "MCP_TOKENS_FILE",
            "MCP_TOOLS",
            "PACK_SETTING",
        ];
        expected.extend(
            ["HOME", "PATH"]
                .into_iter()
                .filter(|k| std::env::var_os(k).is_some()),
        );
        expected.sort();
        let env: Vec<&str> = env
            .iter()
            .map(String::as_str)
            .filter(|k| !matches!(*k, "LC_CTYPE" | "__CF_USER_TEXT_ENCODING"))
            .collect();
        assert_eq!(env, expected);

        alpha.tokens.revoke("ci-a").unwrap();
        assert_eq!(get(alpha.port, &a).0, 401, "a revoked token stops working");

        let port = alpha.port;
        drop(servers);
        assert!(
            !port_open(&format!("127.0.0.1:{port}")),
            "dropping the servers stops them"
        );
    }

    #[test]
    fn a_turn_without_servers_needs_no_valid_holder() {
        assert_eq!(
            grant_all([], "ci-a", "/sandbox/my work").unwrap(),
            Vec::<String>::new()
        );
    }

    fn start_one(bin: &str, args: &[&str]) -> (StartError, std::time::Duration) {
        let _env = crucible::test_support::env_lock();
        let table = BTreeMap::from([(
            "buildit".to_string(),
            McpCfg {
                bin: bin.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: BTreeMap::new(),
                inherit: Vec::new(),
                tools: Vec::new(),
            },
        )]);
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let started = std::time::Instant::now();
        let error = match start(&table, &vars) {
            Ok(_) => panic!("{bin} started"),
            Err(e) => e,
        };
        let elapsed = started.elapsed();
        match error.downcast::<StartError>() {
            Ok(e) => (e, elapsed),
            Err(e) => panic!("not a StartError: {e:#}"),
        }
    }

    #[test]
    fn a_server_that_exits_at_boot_fails_at_once_with_its_last_stderr_lines() {
        let (error, elapsed) = start_one(
            "sh",
            &[
                "-c",
                "for i in $(seq 1 59); do echo \"noise $i\" >&2; done; \
                 printf '%02000d\\n' 0 >&2; \
                 echo 'buildit: no kubeconfig at /var/run/kube' >&2; exit 7",
            ],
        );
        assert!(matches!(error, StartError::Exited { .. }), "{error:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "an exited server fails without waiting out the boot timeout: {elapsed:?}"
        );
        let message = error.to_string();
        assert!(
            message.starts_with("[mcp.buildit] (`sh`) exited (exit status: 7) before listening"),
            "{message}"
        );
        let lines: Vec<&str> = message.lines().skip(1).collect();
        assert_eq!(lines.len(), STDERR_TAIL, "{message}");
        assert_eq!(lines.first(), Some(&"noise 12"), "{message}");
        assert_eq!(lines[STDERR_TAIL - 2], "0".repeat(STDERR_LINE_CHARS));
        assert_eq!(
            lines.last(),
            Some(&"buildit: no kubeconfig at /var/run/kube"),
            "{message}"
        );
    }

    #[test]
    fn stderr_without_newlines_is_capped_and_the_tail_is_redacted() {
        let (error, _) = start_one(
            "sh",
            &[
                "-c",
                "head -c 100000 /dev/zero | tr '\\0' x >&2; \
                 printf '\\nlogin with KUBE_TOKEN=s3cret\\nno newline at exit' >&2; exit 2",
            ],
        );
        let message = error.to_string();
        let lines: Vec<&str> = message.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "x".repeat(STDERR_LINE_CHARS).as_str(),
                "login with KUBE_TOKEN=***",
                "no newline at exit"
            ],
            "{message}"
        );
    }

    #[test]
    fn a_server_that_never_listens_times_out_with_its_stderr() {
        let (error, _) = start_one(
            "sh",
            &["-c", "echo 'waiting on the gateway' >&2; exec sleep 30"],
        );
        assert!(matches!(error, StartError::BootTimeout { .. }), "{error:?}");
        let message = error.to_string();
        assert!(
            message.ends_with("stderr line(s):\nwaiting on the gateway"),
            "{message}"
        );
    }

    #[test]
    fn a_silent_server_says_so_and_a_missing_binary_is_a_start_error() {
        let (error, _) = start_one("sh", &["-c", "exit 1"]);
        assert!(
            error.to_string().ends_with("; it wrote nothing to stderr"),
            "{error}"
        );
        let (error, _) = start_one("/nonexistent/buildit-mcp", &[]);
        assert!(matches!(error, StartError::Spawn { .. }), "{error:?}");
    }
}
