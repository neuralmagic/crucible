//! Start the pack's `[mcp]` servers as children of crucible, one process per server on its own
//! port, and hand each sandbox its own bearer token through the server's token file (see
//! [`crucible_contract::mcp`]).

use crate::control::broker::{mint_token, port_open};
use crate::manifest::McpCfg;
use anyhow::{Context, Result};
use crucible_contract::mcp::{self as wire, TokenHolder, TokenMap};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
enum StartError {
    #[error("[mcp.{key}] cannot listen on port {port}: something already does")]
    PortInUse { key: String, port: u16 },
    #[error("[mcp.{key}] (`{bin}`) did not listen on port {port} within {BOOT_TIMEOUT:?}")]
    BootTimeout { key: String, bin: String, port: u16 },
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
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning [mcp.{key}] (`{}`)", cfg.bin))?;
        let child = Arc::new(KillOnDrop(child));
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while !port_open(&probe) {
            if Instant::now() > deadline {
                let (key, bin) = (key.clone(), cfg.bin.clone());
                return Err(StartError::BootTimeout { key, bin, port }.into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        servers.push(Server {
            key: key.clone(),
            port,
            tokens,
            _child: child,
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

#[cfg(test)]
mod tests {
    use crate::control::broker::port_open;
    use crate::control::mcp::start;
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
}
