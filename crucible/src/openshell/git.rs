//! The run's `crucible-git`, started in the loop pod when the controller granted the run a server
//! served there by `crucible-git serve`: it proxies git to GitHub and opens pull requests with the
//! run's minted App token, which it reads from its file on every request. Sandboxes reach it with
//! run tokens the controller issued for this run; the run's own tasks reach it on loopback. Its
//! grant, egress and `.mcp.json` entry are any agent server's ([`crate::openshell::agent_servers`]).

use anyhow::{Context, Result};
use crucible_contract::agent_servers::GitSite;
use crucible_contract::git::{ENV_TOKEN_FILE, LOCAL_BIND};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
#[error("crucible-git did not start listening on {probe} within {seconds}s")]
struct BootTimeout {
    probe: String,
    seconds: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("crucible-git's run issuer {0:?} is not <issuer>=<jwks uri>")]
pub struct BadIssuer(String);

/// `crucible-git serve` for one run: what the controller granted, served at `url` on `port`.
pub struct Git<'a> {
    pub site: &'a GitSite,
    pub url: &'a str,
    /// The name sandboxes reach the loop pod by.
    pub host: &'a str,
    pub port: u16,
}

impl Git<'_> {
    /// `crucible-git`'s arguments for the run `run_id`.
    pub fn args(&self, run_id: &str) -> Result<Vec<String>, BadIssuer> {
        let jwks = self
            .site
            .run_issuer
            .split_once('=')
            .and_then(|(_, jwks)| url::Url::parse(jwks).ok())
            .ok_or_else(|| BadIssuer(self.site.run_issuer.clone()))?;
        let mut args: Vec<String> = [
            "serve",
            "--bind",
            &format!("0.0.0.0:{}", self.port),
            "--local-bind",
            LOCAL_BIND,
            "--run-issuer",
            &self.site.run_issuer,
            "--resource",
            self.url,
            "--allowed-host",
            self.host,
            "--run-id",
            run_id,
        ]
        .map(String::from)
        .to_vec();
        if jwks.scheme() == "http" {
            args.push("--insecure-jwks-http".to_string());
        }
        Ok(args)
    }

    fn listening(&self) -> bool {
        crate::control::broker::port_open(&format!("127.0.0.1:{}", self.port))
            && crate::control::broker::port_open(LOCAL_BIND)
    }

    /// Start `crucible-git` for this run unless it is already listening, and wait until both its
    /// listeners accept connections. It is the loop pod's child for the run's lifetime.
    pub fn ensure_running(&self) -> Result<()> {
        if self.listening() {
            return Ok(());
        }
        let run_id = std::env::var(crucible_contract::ENV_POD_NAME)
            .context("crucible-git needs the run pod's name to know its run")?;
        if std::env::var_os(ENV_TOKEN_FILE).is_none() {
            tracing::warn!(
                "the run binds no GitHub App token file ({ENV_TOKEN_FILE}); crucible-git refuses every call"
            );
        }
        Command::new("crucible-git")
            .args(self.args(&run_id)?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning crucible-git")?;
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if self.listening() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(BootTimeout {
            probe: format!("127.0.0.1:{} and {LOCAL_BIND}", self.port),
            seconds: BOOT_TIMEOUT.as_secs(),
        }
        .into())
    }
}
