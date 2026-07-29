//! The broker's typed gRPC path to the local OpenShell gateway, replacing `openshell sandbox
//! exec` shell-outs for the sandbox tree hashes. Same pod as the gateway, so the channel
//! bootstrap mirrors crucible's: `https://localhost:17670`, mTLS from the registered gateway's
//! cert dir. Upload/download have no RPC (SSH-tar), so `sandbox download` stays on the CLI.

use openshell_core::auth::EdgeAuthInterceptor;
use openshell_core::proto::open_shell_client::OpenShellClient;
use openshell_core::proto::{
    ExecSandboxRequest, GetSandboxRequest, exec_sandbox_event::Payload as ExecPayload,
};
use std::path::PathBuf;
use std::time::Duration;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// Every way a gateway exec can fail, as matchable variants rather than anyhow strings: callers can
/// tell "no sandbox answered" (the gate's normal post-turn case) from a transport/cert problem.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GatewayError {
    #[error("reading gateway certs in {}: {source}", path.display())]
    Certs {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuring the gateway endpoint: {0}")]
    Endpoint(#[from] tonic::transport::Error),
    #[error("{rpc}({sandbox}): {status}")]
    Rpc {
        rpc: &'static str,
        sandbox: String,
        #[source]
        status: Box<tonic::Status>,
    },
    #[error("sandbox '{name}': {why}")]
    SandboxUnresolved { name: String, why: &'static str },
    #[error("exec_sandbox({name}) stream ended without an exit event")]
    NoExit { name: String },
    #[error("building a fallback runtime for the gateway exec: {0}")]
    Runtime(std::io::Error),
}

/// Gateway bind constants, fixed by the openshell-loop template, mirrored from
/// `crucible::openshell::gateway` (the broker crate can't depend on the engine crate).
const GATEWAY_PORT: u16 = 17670;
const GATEWAY_NAME: &str = "ci";

/// The registered gateway's mTLS client-cert dir, resolved exactly as the CLI does
/// (`XDG_CONFIG_HOME`, else `~/.config`).
fn mtls_dir() -> Result<PathBuf, GatewayError> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var("HOME").map_err(|_| GatewayError::Certs {
                path: PathBuf::from("$HOME"),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "HOME unset; cannot locate gateway certs",
                ),
            })?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base
        .join("openshell")
        .join("gateways")
        .join(GATEWAY_NAME)
        .join("mtls"))
}

fn build_channel() -> Result<Channel, GatewayError> {
    let dir = mtls_dir()?;
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read(&path).map_err(|source| GatewayError::Certs { path, source })
    };
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(read("ca.crt")?))
        .identity(Identity::from_pem(read("tls.crt")?, read("tls.key")?));
    let endpoint = Endpoint::from_shared(format!("https://localhost:{GATEWAY_PORT}"))
        .map_err(GatewayError::Endpoint)?
        .connect_timeout(Duration::from_secs(10))
        .tls_config(tls)
        .map_err(GatewayError::Endpoint)?;
    Ok(endpoint.connect_lazy())
}

/// A completed sandbox exec, streams fully drained.
pub(crate) struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_ok: bool,
}

/// Run `command` in sandbox `name` over gRPC and collect its output. Sync; the broker's tool
/// handlers already run on `spawn_blocking` threads, where the runtime handle is current, so
/// this blocks the blocking thread exactly like the `Command` shell-outs it replaces. Falls back
/// to a private current-thread runtime when no ambient runtime exists (unit tests).
pub(crate) fn exec_collect(name: &str, command: &[String]) -> Result<ExecOutput, GatewayError> {
    let fut = exec_collect_async(name, command);
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(GatewayError::Runtime)?
            .block_on(fut),
    }
}

async fn exec_collect_async(name: &str, command: &[String]) -> Result<ExecOutput, GatewayError> {
    let rpc_err = |rpc: &'static str| {
        let sandbox = name.to_string();
        move |status: tonic::Status| GatewayError::Rpc {
            rpc,
            sandbox,
            status: Box::new(status),
        }
    };
    let channel = build_channel()?;
    let mut client = OpenShellClient::with_interceptor(channel, EdgeAuthInterceptor::noop());

    let sandbox = client
        .get_sandbox(GetSandboxRequest {
            name: name.to_string(),
            ..Default::default()
        })
        .await
        .map_err(rpc_err("get_sandbox"))?
        .into_inner()
        .sandbox
        .ok_or_else(|| GatewayError::SandboxUnresolved {
            name: name.to_string(),
            why: "missing from the get_sandbox response",
        })?;
    let sandbox_id = sandbox
        .metadata
        .map(|m| m.id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| GatewayError::SandboxUnresolved {
            name: name.to_string(),
            why: "has no id",
        })?;

    let mut stream = client
        .exec_sandbox(ExecSandboxRequest {
            sandbox_id,
            command: command.to_vec(),
            ..ExecSandboxRequest::default()
        })
        .await
        .map_err(rpc_err("exec_sandbox"))?
        .into_inner();

    let mut out = ExecOutput {
        stdout: String::new(),
        stderr: String::new(),
        // The exit payload carries the code; anything nonzero (or a broken stream) is not ok.
        exit_ok: false,
    };
    let mut saw_exit = false;
    loop {
        let msg = stream.message().await.map_err(rpc_err("exec_sandbox"))?;
        let Some(event) = msg else { break };
        match event.payload {
            Some(ExecPayload::Stdout(chunk)) => {
                out.stdout.push_str(&String::from_utf8_lossy(&chunk.data));
            }
            Some(ExecPayload::Stderr(chunk)) => {
                out.stderr.push_str(&String::from_utf8_lossy(&chunk.data));
            }
            Some(ExecPayload::Exit(exit)) => {
                saw_exit = true;
                out.exit_ok = exit.exit_code == 0;
            }
            None => {}
        }
    }
    if !saw_exit {
        return Err(GatewayError::NoExit {
            name: name.to_string(),
        });
    }
    Ok(out)
}
