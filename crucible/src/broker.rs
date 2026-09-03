//! Spawn the loop-pod provisioning broker (`[agent.broker].bin`) as a child of crucible.
//!
//! The sandboxed agent has no direct authority to spend GPU / submit a Workload / dispatch CI; it
//! asks this server-side broker, which holds the privilege and the admission logic. crucible spawns
//! it once per run and the sandboxed claude reaches it over streamable-http at
//! `host.containers.internal:8849` (egress-allowlisted, seeded as `.mcp.json` by
//! [`crate::openshell::run`]).
//!
//! Like the openshell gateway (see [`crate::openshell::gateway`]), the broker is a run-lifetime
//! daemon, not handle-held: crucible is the pod entrypoint, so when it exits the container exits and
//! the runtime reaps the child, no teardown needed. We spawn it detached and block until its port
//! accepts a connection, so the first turn's MCP calls never race the boot.

use crate::manifest::BrokerCfg;
use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
#[error("provisioning broker (`{bin}`) did not start listening on {probe} within {seconds}s")]
struct BrokerBootTimeout {
    bin: String,
    probe: String,
    seconds: u64,
}

/// How long to wait for the broker to start listening before giving up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Start the broker if it isn't already listening, blocking until its port accepts a connection.
/// `env` is the manifest's `[agent].env`, forwarded so the broker's backends see the same config
/// the agent does; crucible's own environment (KUBECONFIG, the PR token, ...) is inherited too,
/// including `TRACEPARENT`, deliberately: broker spans grafting onto the run trace is telemetry,
/// not a leak (unlike the agent child, which strips it).
/// `bounds_env` is the frozen manifest's resolved output bounds (`BROKER_OUTPUTS` and friends).
/// `control_port`, when set, is handed to the broker as `BROKER_CONTROL_ADDR` so its approval
/// watcher can send a re-scope back to the loop's control bridge on a granted approval.
///
/// Returns the bearer token guarding the broker's http endpoint. The broker binds `0.0.0.0`, so on
/// a cluster pod its port is reachable from any pod that can route to it, the token (seeded into
/// the sandbox's `.mcp.json` headers, checked by the broker) closes that side while the sandbox
/// keeps its capability. `BROKER_TOKEN` in crucible's env wins (an operator pairing with an
/// externally-started broker); otherwise a fresh random token is minted per run. `None` only when
/// the broker was already listening and no `BROKER_TOKEN` is set, we can't retrofit a token onto a
/// process we didn't start, so that broker stays open (the pre-token behavior).
pub fn ensure_running(
    cfg: &BrokerCfg,
    env: &[(String, String)],
    bounds_env: &[(String, String)],
    control_port: Option<u16>,
    sandbox_name: &str,
) -> Result<Option<String>> {
    let env_token = std::env::var("BROKER_TOKEN").ok().filter(|t| !t.is_empty());
    let probe = probe_addr(&cfg.bind);
    if port_open(&probe) {
        // Already up (idempotent, a re-entry or an externally-started broker).
        return Ok(env_token);
    }
    let token = match env_token {
        Some(t) => t,
        None => mint_token().context("minting the broker bearer token")?,
    };

    let mut cmd = Command::new(&cfg.bin);
    cmd.env("BROKER_NAME", &cfg.name)
        .env("BROKER_BIND", &cfg.bind)
        .env("BROKER_TOKEN", &token)
        // The deep loop's per-workspace sandbox name (the pid in it is ours, crucible spawns the
        // broker), so the broker's build sync downloads from the right sandbox, not a fixed "ci".
        .env("BROKER_SANDBOX_NAME", sandbox_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep the broker's stderr (its one startup line + any error) in crucible's pod log.
        .stderr(Stdio::inherit());
    if let Some(port) = control_port {
        // The bridge binds 0.0.0.0:<port>; the broker is a same-pod child, so loopback reaches it.
        cmd.env("BROKER_CONTROL_ADDR", format!("127.0.0.1:{port}"));
    }
    if cfg.build {
        // Arm the engine-side build tools; the build target comes from the loop pod's
        // FORGE_* env, which the broker inherits along with the rest of crucible's environment.
        cmd.env("BROKER_BUILD", "1");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // The frozen manifest's output bounds, applied after `[agent].env` so a manifest key can
    // never shadow the bounds it is bounded by.
    for (k, v) in bounds_env {
        cmd.env(k, v);
    }
    cmd.spawn()
        .with_context(|| format!("spawning the provisioning broker (`{}`)", cfg.bin))?;

    let deadline = Instant::now() + BOOT_TIMEOUT;
    while Instant::now() < deadline {
        if port_open(&probe) {
            return Ok(Some(token));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(BrokerBootTimeout {
        bin: cfg.bin.clone(),
        probe: probe.to_string(),
        seconds: BOOT_TIMEOUT.as_secs(),
    }
    .into())
}

/// A fresh random bearer token: 24 bytes of OS entropy, hex-encoded. Read from `/dev/urandom`
/// directly so no rand crate is pulled in for one token per run (linux pods + macOS both have it).
fn mint_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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
    use super::*;

    #[test]
    fn probe_addr_maps_bind_to_loopback_port() {
        assert_eq!(probe_addr("0.0.0.0:8849"), "127.0.0.1:8849");
        assert_eq!(probe_addr("0.0.0.0:1234"), "127.0.0.1:1234");
        // No port: passed through unchanged (degenerate, but don't panic).
        assert_eq!(probe_addr("localhost"), "localhost");
    }

    #[test]
    fn port_open_false_on_garbage_and_dead_port() {
        assert!(!port_open("not-an-addr"));
        // Port 1 on loopback is not listening in CI; connect fails fast.
        assert!(!port_open("127.0.0.1:1"));
    }

    /// Real entropy read (no mock): 48 hex chars, and two mints never collide.
    #[test]
    fn mint_token_is_hex_and_unique() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
