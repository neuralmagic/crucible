//! `crucible-controller local`: the controller on one machine, with its own Postgres and no
//! identity provider, Vault, or cluster.
//!
//! ```text
//! local --port 8870 ──▶ state dir + webhook key ──exec──▶ autopilot under [`Local::autopilot`]
//! ```

use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::EMBEDDED_DATABASE_URL;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocalError {
    #[error("--hooks-port {0} is the API port; webhooks need a port of their own")]
    HooksOnApiPort(NonZeroU16),
    #[error("--port {0} leaves no port after it for webhooks; pass --hooks-port")]
    NoPortAfter(NonZeroU16),
    #[error("HOME is unset, so there is no default state directory; pass --state-dir")]
    NoHome,
}

#[derive(Debug, Clone, clap::Args)]
pub struct LocalArgs {
    /// The loopback port of the API and the UI.
    #[arg(long, default_value = "8870")]
    port: NonZeroU16,
    /// The loopback port webhook deliveries arrive on. Defaults to the port after `--port`.
    #[arg(long, env = "HOOKS_PORT")]
    hooks_port: Option<NonZeroU16>,
    /// The login every request acts as, and the only admin.
    #[arg(long, env = "USER", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    identity: String,
    /// Where the database, the webhook key, and the runs live. Defaults to
    /// `$XDG_STATE_HOME/crucible-controller`, else `~/.local/state/crucible-controller`.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

impl LocalArgs {
    /// The local controller that `exe` runs, with the machine's defaults filled in.
    pub fn resolve(self, exe: PathBuf) -> Result<Local> {
        let state = match self.state_dir {
            Some(dir) => dir,
            None => {
                default_state_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))?
            }
        };
        Ok(Local {
            exe,
            state: std::path::absolute(&state)
                .with_context(|| format!("resolving the state directory {}", state.display()))?,
            identity: self.identity,
            port: self.port,
            hooks_port: hooks_port(self.port, self.hooks_port)?,
            podman_socket: match std::env::var_os("OPENSHELL_PODMAN_SOCKET") {
                Some(_) => None,
                None => podman_machine_socket(),
            },
        })
    }
}

fn hooks_port(port: NonZeroU16, explicit: Option<NonZeroU16>) -> Result<NonZeroU16, LocalError> {
    match explicit {
        Some(hooks) if hooks == port => Err(LocalError::HooksOnApiPort(hooks)),
        Some(hooks) => Ok(hooks),
        None => port.checked_add(1).ok_or(LocalError::NoPortAfter(port)),
    }
}

fn default_state_dir(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, LocalError> {
    let xdg = xdg_state_home
        .map(PathBuf::from)
        .filter(|root| root.is_absolute());
    let root = match xdg {
        Some(root) => root,
        None => {
            let home = home
                .filter(|home| !home.is_empty())
                .ok_or(LocalError::NoHome)?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(root.join("crucible-controller"))
}

/// The API socket of the running podman machine, where the engine starts OpenShell sandboxes.
fn podman_machine_socket() -> Option<String> {
    let output = Command::new("podman")
        .args([
            "machine",
            "inspect",
            "--format",
            "{{.ConnectionInfo.PodmanSocket.Path}}",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let socket = String::from_utf8(output.stdout).ok()?;
    let socket = socket.trim();
    (!socket.is_empty()).then(|| socket.to_string())
}

/// The key that seals `hmac_sha256` webhook secrets, generated on first use and kept after.
fn webhook_key(state: &Path) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    use base64::Engine;

    let path = state.join("webhook.key");
    match std::fs::read_to_string(&path) {
        Ok(key) if !key.trim().is_empty() => return Ok(path),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let mut material = [0u8; 32];
    rand::fill(&mut material[..]);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    file.write_all(
        base64::engine::general_purpose::STANDARD
            .encode(material)
            .as_bytes(),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[derive(Debug)]
pub struct Local {
    exe: PathBuf,
    state: PathBuf,
    identity: String,
    port: NonZeroU16,
    hooks_port: NonZeroU16,
    podman_socket: Option<String>,
}

impl Local {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn hooks_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.hooks_port)
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// `autopilot` as the local controller runs it. Creates the state directory and the webhook
    /// key on first use.
    pub fn autopilot(&self) -> Result<Command> {
        std::fs::create_dir_all(&self.state)
            .with_context(|| format!("creating the state directory {}", self.state.display()))?;
        let key = webhook_key(&self.state)?;
        let mut command = Command::new(&self.exe);
        command
            .arg("autopilot")
            .current_dir(&self.state)
            .env_remove("CONTROLLER_API_TOKEN")
            .env_remove("CONTROLLER_OIDC_ISSUER")
            .env_remove("VAULT_ADDR")
            .env("KUBECONFIG", "/dev/null")
            .env("DATABASE_URL", EMBEDDED_DATABASE_URL)
            .env("CONTROLLER_STATE_DIR", &self.state)
            .env("CONTROLLER_API_ADDR", format!("127.0.0.1:{}", self.port))
            .env("CONTROLLER_PUBLIC_URL", self.url())
            .env(
                "CONTROLLER_HOOKS_ADDR",
                format!("127.0.0.1:{}", self.hooks_port),
            )
            .env("CONTROLLER_HOOKS_PUBLIC_URL", self.hooks_url())
            .env("CONTROLLER_WEBHOOK_KEY_FILE", key)
            .env("CONTROLLER_DEV_IDENTITY", &self.identity)
            .env("CONTROLLER_ADMINS", &self.identity)
            .env("CONTROLLER_SESSION_SECURE", "false")
            .env("CONTROLLER_PLAYBOOK_EXECUTOR", "local")
            .env("CONTROLLER_SCOPE_EXECUTOR", "disabled");
        if let Some(engine) = self
            .exe
            .parent()
            .map(|dir| dir.join("crucible"))
            .filter(|engine| engine.is_file())
        {
            command.env("CRUCIBLE_BIN", engine);
        }
        if let Some(socket) = &self.podman_socket {
            command.env("OPENSHELL_PODMAN_SOCKET", socket);
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::num::NonZeroU16;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use crate::local::{Local, LocalArgs, LocalError, default_state_dir, hooks_port, webhook_key};

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: LocalArgs,
    }

    fn port(n: u16) -> NonZeroU16 {
        NonZeroU16::new(n).unwrap()
    }

    fn local(dir: &Path) -> Local {
        Local {
            exe: dir.join("bin/crucible-controller"),
            state: dir.join("state"),
            identity: "wren".into(),
            port: port(9870),
            hooks_port: port(9871),
            podman_socket: None,
        }
    }

    /// What `command` sets (`Some`) and removes (`None`).
    fn env(command: &std::process::Command) -> BTreeMap<String, Option<OsString>> {
        command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(OsStr::to_os_string),
                )
            })
            .collect()
    }

    #[test]
    fn the_flags_parse_with_the_documented_defaults() {
        let args = Harness::try_parse_from(["local", "--identity", "wren"])
            .unwrap()
            .args;
        assert_eq!(args.port, port(8870));
        assert_eq!(args.identity, "wren");
        assert_eq!(args.state_dir, None);

        let args = Harness::try_parse_from([
            "local",
            "--identity",
            "wren",
            "--port",
            "9000",
            "--hooks-port",
            "9100",
            "--state-dir",
            "/var/tmp/controller",
        ])
        .unwrap()
        .args;
        assert_eq!(args.port, port(9000));
        assert_eq!(args.hooks_port, Some(port(9100)));
        assert_eq!(args.state_dir, Some(PathBuf::from("/var/tmp/controller")));
    }

    #[test]
    fn ports_outside_the_range_and_an_empty_identity_are_refused() {
        for bad in ["0", "65536", "-1", "localhost", ""] {
            assert!(
                Harness::try_parse_from(["local", "--identity", "wren", "--port", bad]).is_err(),
                "--port {bad:?}"
            );
            assert!(
                Harness::try_parse_from(["local", "--identity", "wren", "--hooks-port", bad])
                    .is_err(),
                "--hooks-port {bad:?}"
            );
        }
        assert!(Harness::try_parse_from(["local", "--identity", ""]).is_err());
    }

    #[test]
    fn webhooks_default_to_the_port_after_the_api() {
        assert_eq!(hooks_port(port(8870), None).unwrap(), port(8871));
        assert_eq!(hooks_port(port(9000), None).unwrap(), port(9001));
        assert_eq!(
            hooks_port(port(9000), Some(port(7000))).unwrap(),
            port(7000)
        );
        assert_eq!(
            hooks_port(port(65535), Some(port(7000))).unwrap(),
            port(7000)
        );

        assert_eq!(
            hooks_port(port(65535), None),
            Err(LocalError::NoPortAfter(port(65535)))
        );
        assert_eq!(
            hooks_port(port(9000), Some(port(9000))),
            Err(LocalError::HooksOnApiPort(port(9000)))
        );
        assert_eq!(
            LocalError::NoPortAfter(port(65535)).to_string(),
            "--port 65535 leaves no port after it for webhooks; pass --hooks-port"
        );
    }

    #[test]
    fn the_state_directory_follows_xdg_and_falls_back_to_home() {
        let os = |text: &str| Some(OsString::from(text));
        assert_eq!(
            default_state_dir(os("/xdg/state"), os("/home/wren")).unwrap(),
            PathBuf::from("/xdg/state/crucible-controller")
        );
        for ignored in [None, os(""), os("relative/state")] {
            assert_eq!(
                default_state_dir(ignored, os("/home/wren")).unwrap(),
                PathBuf::from("/home/wren/.local/state/crucible-controller")
            );
        }
        for home in [None, os("")] {
            assert_eq!(default_state_dir(None, home), Err(LocalError::NoHome));
        }
    }

    #[test]
    fn the_webhook_key_is_generated_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = webhook_key(dir.path()).unwrap();
        assert_eq!(path, dir.path().join("webhook.key"));
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let keys = crate::launches::webhooks::keys::WebhookKeys::from_sources(
            Some(path.to_string_lossy().into_owned()),
            None,
        );
        assert!(keys.unwrap().is_some());

        assert_eq!(webhook_key(dir.path()).unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn an_empty_webhook_key_is_replaced_and_made_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webhook.key");
        for blank in ["", " \n"] {
            std::fs::write(&path, blank).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            webhook_key(dir.path()).unwrap();
            assert!(!std::fs::read_to_string(&path).unwrap().trim().is_empty());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn autopilot_runs_open_on_loopback_with_no_cluster_vault_or_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let local = local(dir.path());
        let command = local.autopilot().unwrap();

        assert_eq!(command.get_program(), local.exe.as_os_str());
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["autopilot"]);
        assert_eq!(command.get_current_dir(), Some(local.state.as_path()));
        assert!(local.state.is_dir());

        let set = |value: &str| Some(OsString::from(value));
        let expected: BTreeMap<String, Option<OsString>> = [
            ("CONTROLLER_API_TOKEN", None),
            ("CONTROLLER_OIDC_ISSUER", None),
            ("VAULT_ADDR", None),
            ("KUBECONFIG", set("/dev/null")),
            ("DATABASE_URL", set("embedded")),
            (
                "CONTROLLER_STATE_DIR",
                Some(local.state.clone().into_os_string()),
            ),
            ("CONTROLLER_API_ADDR", set("127.0.0.1:9870")),
            ("CONTROLLER_PUBLIC_URL", set("http://127.0.0.1:9870")),
            ("CONTROLLER_HOOKS_ADDR", set("127.0.0.1:9871")),
            ("CONTROLLER_HOOKS_PUBLIC_URL", set("http://127.0.0.1:9871")),
            (
                "CONTROLLER_WEBHOOK_KEY_FILE",
                Some(local.state.join("webhook.key").into_os_string()),
            ),
            ("CONTROLLER_DEV_IDENTITY", set("wren")),
            ("CONTROLLER_ADMINS", set("wren")),
            ("CONTROLLER_SESSION_SECURE", set("false")),
            ("CONTROLLER_PLAYBOOK_EXECUTOR", set("local")),
            ("CONTROLLER_SCOPE_EXECUTOR", set("disabled")),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
        assert_eq!(env(&command), expected);
    }

    #[test]
    fn autopilot_uses_the_engine_beside_the_controller_and_the_podman_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = local(dir.path());
        let engine = dir.path().join("bin/crucible");
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(&engine, "").unwrap();
        local.podman_socket = Some("/run/podman/api.sock".into());

        let env = env(&local.autopilot().unwrap());
        assert_eq!(env["CRUCIBLE_BIN"], Some(engine.into_os_string()));
        assert_eq!(
            env["OPENSHELL_PODMAN_SOCKET"],
            Some(OsString::from("/run/podman/api.sock"))
        );
    }

    #[test]
    fn the_settings_autopilot_gets_parse_as_a_controller_config() {
        let dir = tempfile::tempdir().unwrap();
        let command = local(dir.path()).autopilot().unwrap();
        let flags: Vec<String> = [
            ("--state-dir", "CONTROLLER_STATE_DIR"),
            ("--db", "DATABASE_URL"),
            ("--public-url", "CONTROLLER_PUBLIC_URL"),
            ("--hooks-public-url", "CONTROLLER_HOOKS_PUBLIC_URL"),
            ("--admin", "CONTROLLER_ADMINS"),
            ("--playbook-executor", "CONTROLLER_PLAYBOOK_EXECUTOR"),
            ("--scope-executor", "CONTROLLER_SCOPE_EXECUTOR"),
        ]
        .into_iter()
        .flat_map(|(flag, name)| {
            let value = env(&command)[name].clone().unwrap();
            [flag.to_string(), value.to_string_lossy().into_owned()]
        })
        .collect();
        let cfg = crate::testing::cfg_from_args(
            std::iter::once("ctl".to_string()).chain(flags.iter().cloned()),
        );
        assert!(cfg.wants_embedded_db());
        assert_eq!(cfg.state_dir, dir.path().join("state"));
        assert_eq!(cfg.admins, ["wren"]);
        assert_eq!(cfg.playbook_executor, crate::PlaybookExecutor::Local);
        assert_eq!(cfg.scope_executor, crate::ScopeExecutor::Disabled);
        assert_eq!(cfg.public_url.as_deref(), Some("http://127.0.0.1:9870"));
        assert_eq!(
            cfg.hooks_public_url.as_deref(),
            Some("http://127.0.0.1:9871")
        );
    }
}
