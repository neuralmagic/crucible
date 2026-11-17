//! One session's sandbox on the local OpenShell gateway, over the gateway's gRPC API.

use std::path::{Path, PathBuf};

use crucible::manifest::SandboxResources;
use crucible::openshell::grpc::{
    ExecResult, Gateway, GatewayDial, HealthProbe, MIN_GATEWAY_VERSION, ProviderName, VersionGate,
    check_gateway_version,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::net::Handle;

/// Sandbox names start with this, so a leftover one can be told from someone else's.
pub const PREFIX: &str = "cr-draft-";
/// This machine as a sandbox reaches it: the name the gateway maps for its own policy.
pub const HOST: &str = "host.openshell.internal";

#[derive(Deserialize)]
struct Registered {
    gateway_endpoint: String,
}

/// Where the OpenShell CLI keeps its gateways: `$XDG_CONFIG_HOME/openshell`, else under `~/.config`.
fn config_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(".config"),
    };
    Ok(base.join("openshell"))
}

/// The gateway the OpenShell CLI has active under `dir`, and its client certificate.
fn dial_in(dir: &Path) -> Result<GatewayDial, String> {
    let missing = "No OpenShell gateway is registered on this machine";
    let name = std::fs::read_to_string(dir.join("active_gateway")).map_err(|_| missing)?;
    let gateway = dir.join("gateways").join(name.trim());
    let metadata = std::fs::read(gateway.join("metadata.json")).map_err(|_| missing)?;
    let registered: Registered = serde_json::from_slice(&metadata)
        .map_err(|e| format!("reading gateway {}: {e}", name.trim()))?;
    Ok(GatewayDial {
        endpoint: registered.gateway_endpoint,
        tls_dir: gateway.join("mtls"),
    })
}

/// The policy line that lets a sandbox reach the relay on `port`.
fn relay_endpoint(port: u16) -> String {
    format!("{HOST}:{port}:read-write:rest:enforce")
}

/// Where the names of sandboxes this app created are kept until each is deleted, so one that
/// outlived the app is deleted by the next session.
fn ledger() -> Option<PathBuf> {
    Some(crate::draft::root(None)?.parent()?.join("agent-sandboxes"))
}

fn listed(ledger: &Path) -> Vec<String> {
    std::fs::read_to_string(ledger)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|name| name.starts_with(PREFIX))
        .map(str::to_string)
        .collect()
}

fn list(ledger: &Path, names: &[String]) {
    if names.is_empty() {
        let _ = std::fs::remove_file(ledger);
        return;
    }
    if let Some(parent) = ledger.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(ledger, names.join("\n") + "\n");
}

fn record(ledger: &Path, name: &str) {
    let mut names = listed(ledger);
    names.push(name.to_string());
    list(ledger, &names);
}

fn forget(ledger: &Path, name: &str) {
    let mut names = listed(ledger);
    names.retain(|kept| kept != name);
    list(ledger, &names);
}

/// Delete the sandboxes an earlier run of the app left behind.
async fn sweep(gateway: &Gateway, ledger: &Path) {
    for name in listed(ledger) {
        if gateway.delete_sandbox(&name).await.is_ok() {
            forget(ledger, &name);
        }
    }
}

/// A running sandbox. Dropping it deletes it, and everything running in it.
pub struct Sandbox {
    name: String,
    gateway: Gateway,
    runtime: Handle,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let (gateway, name) = (self.gateway.clone(), self.name.clone());
        self.runtime.spawn(async move {
            if gateway.delete_sandbox(&name).await.is_ok()
                && let Some(ledger) = ledger()
            {
                forget(&ledger, &name);
            }
        });
    }
}

/// Create sandbox `name` from `image` with `providers` attached, and let `binaries` in it reach
/// the relay on `port`. Returns once it is ready to exec in.
pub async fn create(
    runtime: &Handle,
    name: &str,
    image: &str,
    providers: &[String],
    binaries: &[String],
    port: u16,
) -> Result<Sandbox, String> {
    if binaries.is_empty() {
        return Err("[agent].binaries names nothing that may reach the controller".into());
    }
    let dial = dial_in(&config_dir()?)?;
    let down = || format!("The OpenShell gateway at {} did not answer", dial.endpoint);
    let version = HealthProbe::new(&dial)
        .ok_or_else(down)?
        .report_version()
        .await
        .ok_or_else(down)?;
    if let VersionGate::TooOld { reported } = check_gateway_version(&version) {
        let (major, minor, patch) = MIN_GATEWAY_VERSION;
        return Err(format!(
            "The OpenShell gateway is {reported}; drafting with an agent needs {major}.{minor}.{patch} or newer"
        ));
    }
    let gateway = Gateway::connect(&dial).map_err(|e| format!("{e:#}"))?;
    let providers: Vec<ProviderName> = providers.iter().map(ProviderName::named).collect();
    if let Some(ledger) = ledger() {
        sweep(&gateway, &ledger).await;
        record(&ledger, name);
    }
    gateway
        .create_sandbox(
            name,
            Some(image),
            &providers,
            &[],
            &[],
            &SandboxResources::default(),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
    let sandbox = Sandbox {
        name: name.to_string(),
        gateway,
        runtime: runtime.clone(),
    };
    sandbox
        .gateway
        .update_policy_wait(name, binaries, &[relay_endpoint(port)], &[])
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(sandbox)
}

impl Sandbox {
    /// Run `argv` to its end or until `cancel`, handing each stdout line to `on_line`.
    pub async fn exec(
        &self,
        argv: &[String],
        cancel: &CancellationToken,
        on_line: impl FnMut(&str),
    ) -> Result<ExecResult, String> {
        self.gateway
            .exec(&self.name, argv, cancel, on_line)
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::sandbox::{dial_in, forget, listed, record, relay_endpoint};

    #[test]
    fn the_active_gateway_is_dialed_with_its_own_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            dial_in(root),
            Err("No OpenShell gateway is registered on this machine".into())
        );
        std::fs::write(root.join("active_gateway"), "openshell\n").unwrap();
        assert!(dial_in(root).is_err());
        let gateway = root.join("gateways/openshell");
        std::fs::create_dir_all(&gateway).unwrap();
        std::fs::write(
            gateway.join("metadata.json"),
            r#"{"name":"openshell","gateway_endpoint":"https://127.0.0.1:17670","is_remote":false,"gateway_port":0,"auth_mode":"mtls"}"#,
        )
        .unwrap();
        let dial = dial_in(root).unwrap();
        assert_eq!(dial.endpoint, "https://127.0.0.1:17670");
        assert_eq!(dial.tls_dir, gateway.join("mtls"));

        std::fs::write(gateway.join("metadata.json"), "{}").unwrap();
        assert!(
            dial_in(root)
                .unwrap_err()
                .starts_with("reading gateway openshell: ")
        );
    }

    #[test]
    fn the_relay_is_the_only_endpoint_granted() {
        assert_eq!(
            relay_endpoint(41234),
            "host.openshell.internal:41234:read-write:rest:enforce"
        );
    }

    #[test]
    fn the_ledger_keeps_each_sandbox_until_it_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("state/agent-sandboxes");
        assert!(listed(&ledger).is_empty());
        record(&ledger, "cr-draft-00000001");
        record(&ledger, "cr-draft-00000002");
        assert_eq!(listed(&ledger), ["cr-draft-00000001", "cr-draft-00000002"]);
        forget(&ledger, "cr-draft-00000001");
        assert_eq!(listed(&ledger), ["cr-draft-00000002"]);
        forget(&ledger, "cr-draft-00000002");
        assert!(!ledger.exists());

        std::fs::write(&ledger, "someone-elses\ncr-draft-00000003\n\n").unwrap();
        assert_eq!(listed(&ledger), ["cr-draft-00000003"]);
    }
}
