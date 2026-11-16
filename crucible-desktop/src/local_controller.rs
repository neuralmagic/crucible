use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub const FLAG: &str = "--local-controller";

#[derive(Debug, thiserror::Error)]
enum LocalError {
    #[error(
        "Playbooks runtime in {directory} requires crucible-controller, crucible, and crux; reinstall the complete application"
    )]
    MissingRuntime { directory: PathBuf },
    #[error("{name} {value:?} is invalid; use a port between 1 and 65535")]
    InvalidPort {
        name: &'static str,
        value: String,
        source: std::num::ParseIntError,
    },
    #[error("local controller identity USER is empty; set USER to your login")]
    EmptyIdentity,
}

fn port(name: &'static str, value: &str) -> Result<NonZeroU16, LocalError> {
    value.parse().map_err(|source| LocalError::InvalidPort {
        name,
        value: value.into(),
        source,
    })
}

fn runtime_dir(exe: &Path, repo: &Path) -> Result<PathBuf> {
    let beside = exe.parent().context("locating the Playbooks runtime")?;
    let complete = |dir: &Path| {
        ["crucible-controller", "crucible", "crux"]
            .iter()
            .all(|name| dir.join(name).is_file())
    };
    if complete(beside) {
        return Ok(beside.to_path_buf());
    }
    // Only unbundled Cargo builds may use a checkout's binaries.
    if beside
        .file_name()
        .is_some_and(|name| name == "debug" || name == "release")
    {
        for profile in ["release", "debug"] {
            let candidate = repo.join("target").join(profile);
            if complete(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(LocalError::MissingRuntime {
        directory: beside.to_path_buf(),
    }
    .into())
}

struct LocalController {
    bin: PathBuf,
    state: PathBuf,
    identity: String,
    port: NonZeroU16,
    hooks_port: NonZeroU16,
}

impl LocalController {
    fn prepare(&self) -> Result<Command> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        use base64::Engine;
        use rand::RngExt;

        std::fs::create_dir_all(&self.state).with_context(|| {
            format!(
                "creating controller state directory {}",
                self.state.display()
            )
        })?;
        let key = self.state.join("webhook.key");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key)
        {
            Ok(mut file) => {
                let bytes: [u8; 32] = rand::rng().random();
                let secret = base64::engine::general_purpose::STANDARD.encode(bytes);
                file.write_all(secret.as_bytes())
                    .context("writing the local controller webhook key")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("creating the local controller webhook key"),
        }
        let path = std::env::join_paths(std::iter::once(self.bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .context("adding the bundled runtime to PATH")?;
        let mut command = Command::new(self.bin.join("crucible-controller"));
        command
            .arg("autopilot")
            .current_dir(&self.state)
            .env_remove("CONTROLLER_API_TOKEN")
            .env_remove("CONTROLLER_OIDC_ISSUER")
            .env_remove("VAULT_ADDR")
            .env("PATH", path)
            .env("KUBECONFIG", "/dev/null")
            .env("DATABASE_URL", "embedded")
            .env("CONTROLLER_API_ADDR", format!("127.0.0.1:{}", self.port))
            .env(
                "CONTROLLER_PUBLIC_URL",
                format!("http://127.0.0.1:{}", self.port),
            )
            .env(
                "CONTROLLER_HOOKS_ADDR",
                format!("127.0.0.1:{}", self.hooks_port),
            )
            .env(
                "CONTROLLER_HOOKS_PUBLIC_URL",
                format!("http://127.0.0.1:{}", self.hooks_port),
            )
            .env("CONTROLLER_WEBHOOK_KEY_FILE", key)
            .env("CONTROLLER_DEV_IDENTITY", &self.identity)
            .env("CONTROLLER_ADMINS", &self.identity)
            .env("CONTROLLER_SESSION_SECURE", "false")
            .env("CONTROLLER_PLAYBOOK_EXECUTOR", "local")
            .env("CONTROLLER_SCOPE_EXECUTOR", "disabled")
            .env("CONTROLLER_STATE_DIR", &self.state)
            .env("CRUCIBLE_BIN", self.bin.join("crucible"));
        if std::env::var_os("OPENSHELL_PODMAN_SOCKET").is_none()
            && let Ok(output) = Command::new("podman")
                .args([
                    "machine",
                    "inspect",
                    "--format",
                    "{{.ConnectionInfo.PodmanSocket.Path}}",
                ])
                .output()
            && output.status.success()
            && let Ok(socket) = String::from_utf8(output.stdout)
            && !socket.trim().is_empty()
        {
            command.env("OPENSHELL_PODMAN_SOCKET", socket.trim());
        }
        Ok(command)
    }
}

pub fn run() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let port_value = std::env::args()
        .nth(2)
        .context("--local-controller requires a loopback port")?;
    let hooks_value = match std::env::var("HOOKS_PORT") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "8871".into(),
        Err(error) => return Err(error).context("reading HOOKS_PORT for the local controller"),
    };
    let exe = std::env::current_exe().context("locating the Playbooks executable")?;
    let state_root = match std::env::var_os("XDG_STATE_HOME") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .context("locating the controller state directory: HOME is unset")?,
        )
        .join(".local/state"),
    };
    let local = LocalController {
        bin: runtime_dir(&exe, &crate::model::default_repo())?,
        state: std::path::absolute(state_root.join("crucible-controller"))
            .context("resolving the controller state directory")?,
        identity: std::env::var("USER")
            .context("selecting the local controller identity: USER is unset")?,
        port: port("local controller port", &port_value)?,
        hooks_port: port("HOOKS_PORT", &hooks_value)?,
    };
    if local.identity.trim().is_empty() {
        return Err(LocalError::EmptyIdentity.into());
    }
    Err(local.prepare()?.exec()).context("starting the bundled local controller")
}

#[cfg(test)]
mod tests {
    use crate::local_controller::{LocalController, port, runtime_dir};

    #[test]
    fn invalid_ports_name_the_refused_value_and_the_allowed_range() {
        for value in ["0", "65536", "localhost", "-1"] {
            let error = port("HOOKS_PORT", value).unwrap_err().to_string();
            assert!(error.contains(value), "{error}");
            assert!(error.contains("between 1 and 65535"), "{error}");
        }
    }

    #[test]
    fn an_installed_runtime_does_not_need_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("Crucible Playbooks.app/Contents/MacOS");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["crucible-controller", "crucible", "crux"] {
            std::fs::write(bin.join(name), "").unwrap();
        }
        assert_eq!(
            runtime_dir(&bin.join("crucible-playbooks"), &dir.path().join("missing")).unwrap(),
            bin
        );
    }

    #[test]
    fn an_incomplete_package_refuses_to_build_from_source() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        let dev_bin = repo.join("target/release");
        std::fs::create_dir_all(&dev_bin).unwrap();
        for name in ["crucible-controller", "crucible", "crux"] {
            std::fs::write(dev_bin.join(name), "").unwrap();
        }
        let error =
            runtime_dir(&dir.path().join("Contents/MacOS/crucible-playbooks"), &repo).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reinstall the complete application")
        );
    }

    #[test]
    fn the_bundled_controller_runs_outside_the_checkout_with_persistent_state() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("runtime with spaces");
        std::fs::create_dir(&bin).unwrap();
        let controller = bin.join("crucible-controller");
        std::fs::write(
            &controller,
            r#"#!/bin/sh
set -eu
test "$1" = autopilot
test . -ef "$CONTROLLER_STATE_DIR"
test "$DATABASE_URL" = embedded
test "$CONTROLLER_API_ADDR" = 127.0.0.1:9870
test "$CONTROLLER_HOOKS_ADDR" = 127.0.0.1:9871
test "$CONTROLLER_PLAYBOOK_EXECUTOR" = local
test "$CONTROLLER_SCOPE_EXECUTOR" = disabled
test "$KUBECONFIG" = /dev/null
test -z "${CONTROLLER_API_TOKEN:-}"
test -z "${CONTROLLER_OIDC_ISSUER:-}"
test -z "${VAULT_ADDR:-}"
test "$(command -v crucible)" = "$CRUCIBLE_BIN"
test -s "$CONTROLLER_WEBHOOK_KEY_FILE"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(bin.join("crucible"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(bin.join("crucible"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let local = LocalController {
            bin,
            state: dir.path().join("state"),
            identity: "local".into(),
            port: "9870".parse().unwrap(),
            hooks_port: "9871".parse().unwrap(),
        };
        assert!(local.prepare().unwrap().status().unwrap().success());
        let key = local.state.join("webhook.key");
        let first = std::fs::read(&key).unwrap();
        assert_eq!(std::fs::metadata(&key).unwrap().mode() & 0o777, 0o600);
        assert!(local.prepare().unwrap().status().unwrap().success());
        assert_eq!(std::fs::read(&key).unwrap(), first);
    }
}
