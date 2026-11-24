//! In-Rust OpenShell backend: run a sandboxed agent turn against the local OpenShell gateway.
//! The control-plane boundary is the gateway's native gRPC API ([`grpc`]), typed Health /
//! sandbox / provider / policy / exec / logs RPCs. Two things stay on the `openshell` CLI
//! because no RPC covers them: file upload/download (SSH-tar) and the gateway/podman boot.
//!
//! Module map:
//! - [`endpoint`]: one egress endpoint, parsed once for the policy, the gateway, and the disclosure.
//! - [`grpc`]: the tonic channel, mTLS setup, and the typed per-turn gateway client.
//! - `provider`: mint the Vertex token from ADC (`gcp_auth`) per turn (constants + minting).
//! - [`placement`]: where a sandbox that asks for GPUs is scheduled.
//! - `policy`: resolve the egress allowlist (endpoints + binaries) from the manifest.
//! - `sandbox`: the per-workspace sandbox name + the surviving CLI upload/download argv.
//! - `gateway`: boot/teardown the local gateway + rootless podman socket (still subprocess).
//! - `run`: the per-turn flow: env script, prompt-over-stdin, exec, download.

pub mod buildit;
pub mod endpoint;
pub mod gateway;
pub mod grpc;
pub mod placement;
pub mod policy;

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// True if a TCP connection to `addr` succeeds within a short timeout.
pub fn port_open(addr: &str) -> bool {
    addr.parse::<SocketAddr>()
        .ok()
        .and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).ok())
        .is_some()
}

/// The host and port an http(s) server URL names, or why it names none.
pub fn http_host_port(url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("the scheme must be http or https".to_string());
    }
    let host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or("it has no host")?
        .to_string();
    let port = parsed.port_or_known_default().ok_or("it has no port")?;
    Ok((host, port))
}
