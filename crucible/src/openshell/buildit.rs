//! The run's buildit, started in the loop pod when the controller granted the run a server served
//! there by `buildit mcp`: it runs builder pods in the profile's buildit namespace from the run's
//! own sandboxes, through the run's gateway, and takes only run tokens the controller issued for
//! this run. Its grant, egress and `.mcp.json` entry are any agent server's
//! ([`crate::openshell::agent_servers`]).

use anyhow::{Context, Result};
use crucible_contract::agent_servers::BuilditSite;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const POLICY: &str = include_str!("../../../buildit/deploy/buildit-policy.toml");

const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// The loop pod's push authfile (`[secrets] push_authfile`).
const ENV_PUSH_AUTHFILE: &str = "FORGE_AUTHFILE";

#[derive(Debug, thiserror::Error)]
#[error("buildit did not start listening on {probe} within {seconds}s")]
struct BootTimeout {
    probe: String,
    seconds: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("buildit's run issuer {0:?} is not <issuer>=<jwks uri>")]
pub struct BadIssuer(String);

/// `buildit mcp` for one run: what the controller granted, served at `url` on `port`.
pub struct Buildit<'a> {
    pub site: &'a BuilditSite,
    pub url: &'a str,
    /// The name sandboxes reach the loop pod by.
    pub host: &'a str,
    pub port: u16,
}

/// What `buildit mcp` runs as for this run.
#[derive(Debug, PartialEq, Eq)]
pub struct RunSite<'a> {
    /// The run pod's name, the `run_id` claim of this run's tokens.
    pub run_id: &'a str,
    pub push_authfile: Option<&'a str>,
}

impl Buildit<'_> {
    /// `buildit mcp`'s arguments for `run`, reading its tool policy from `policy`.
    pub fn args(&self, run: &RunSite<'_>, policy: &str) -> Result<Vec<String>, BadIssuer> {
        let jwks = self
            .site
            .run_issuer
            .split_once('=')
            .and_then(|(_, jwks)| url::Url::parse(jwks).ok())
            .ok_or_else(|| BadIssuer(self.site.run_issuer.clone()))?;
        let mut args: Vec<String> = [
            "mcp",
            "--bind",
            &format!("0.0.0.0:{}", self.port),
            "--run-issuer",
            &self.site.run_issuer,
            "--resource",
            self.url,
            "--allowed-host",
            self.host,
            "--tool-policy",
            policy,
            "--run-id",
            run.run_id,
            "--namespace",
            &self.site.namespace,
        ]
        .map(String::from)
        .to_vec();
        if jwks.scheme() == "http" {
            args.push("--insecure-jwks-http".to_string());
        }
        if let (Some(repo), Some(authfile)) = (&self.site.push_repo, run.push_authfile) {
            args.extend(["--push-repo", repo, "--push-authfile", authfile].map(String::from));
        }
        Ok(args)
    }

    /// Start buildit for this run unless it is already listening, and wait until it accepts
    /// connections. It is the loop pod's child for the run's lifetime, like the broker.
    pub fn ensure_running(&self) -> Result<()> {
        let probe = format!("127.0.0.1:{}", self.port);
        if crate::control::broker::port_open(&probe) {
            return Ok(());
        }
        let run_id = std::env::var(crucible_contract::ENV_POD_NAME)
            .context("buildit needs the run pod's name to know its run")?;
        let push_authfile = std::env::var(ENV_PUSH_AUTHFILE).ok();
        if self.site.push_repo.is_some() && push_authfile.is_none() {
            tracing::warn!(
                "buildit has a push repository but the loop pod has no push authfile; it pushes nothing"
            );
        }
        let policy = std::env::temp_dir().join("buildit-policy.toml");
        std::fs::write(&policy, POLICY).with_context(|| format!("writing {}", policy.display()))?;
        let run = RunSite {
            run_id: &run_id,
            push_authfile: push_authfile.as_deref(),
        };
        Command::new("buildit")
            .args(self.args(&run, &policy.to_string_lossy())?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning buildit")?;
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if crate::control::broker::port_open(&probe) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(BootTimeout {
            probe,
            seconds: BOOT_TIMEOUT.as_secs(),
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::buildit::*;

    const ISSUER: &str =
        "https://crucible.hub.example=https://crucible-ext.hub.example/.well-known/jwks.json";
    const URL: &str = "http://host.openshell.internal:8848/mcp";

    fn site(issuer: &str, push: Option<&str>) -> BuilditSite {
        BuilditSite {
            run_issuer: issuer.into(),
            namespace: "crucible-builds".into(),
            push_repo: push.map(str::to_string),
        }
    }

    fn buildit(site: &BuilditSite) -> Buildit<'_> {
        Buildit {
            site,
            url: URL,
            host: "host.openshell.internal",
            port: 8848,
        }
    }

    #[test]
    fn buildit_serves_this_run_alone_and_pushes_only_with_a_repo_and_an_authfile() {
        let run = RunSite {
            run_id: "crucible-run-brave-otter",
            push_authfile: Some("/etc/quay/push.json"),
        };
        let plain = site(ISSUER, None);
        let args = buildit(&plain).args(&run, "/tmp/policy.toml").unwrap();
        let flag = |args: &[String], f: &str| {
            args.iter()
                .position(|a| a == f)
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(
            flag(&args, "--run-id").as_deref(),
            Some("crucible-run-brave-otter")
        );
        assert_eq!(flag(&args, "--run-issuer").as_deref(), Some(ISSUER));
        assert_eq!(flag(&args, "--resource").as_deref(), Some(URL));
        assert_eq!(
            flag(&args, "--allowed-host").as_deref(),
            Some("host.openshell.internal")
        );
        assert_eq!(
            flag(&args, "--namespace").as_deref(),
            Some("crucible-builds")
        );
        assert_eq!(flag(&args, "--bind").as_deref(), Some("0.0.0.0:8848"));
        assert!(
            !args.contains(&"--push-repo".to_string()),
            "no repo: {args:?}"
        );
        assert!(!args.contains(&"--insecure-jwks-http".to_string()));

        let pushing = site(ISSUER, Some("quay.io/example/crucible-smoke"));
        let args = buildit(&pushing).args(&run, "/p").unwrap();
        assert_eq!(
            flag(&args, "--push-repo").as_deref(),
            Some("quay.io/example/crucible-smoke")
        );
        assert_eq!(
            flag(&args, "--push-authfile").as_deref(),
            Some("/etc/quay/push.json")
        );
        let no_authfile = RunSite {
            push_authfile: None,
            ..run
        };
        assert!(
            !buildit(&pushing)
                .args(&no_authfile, "/p")
                .unwrap()
                .contains(&"--push-repo".to_string())
        );

        let over_http = site(
            "http://ctl.ns.svc:8080=http://ctl.ns.svc:8080/.well-known/jwks.json",
            None,
        );
        assert!(
            buildit(&over_http)
                .args(&no_authfile, "/p")
                .unwrap()
                .contains(&"--insecure-jwks-http".to_string())
        );
    }

    #[test]
    fn its_port_and_url_are_the_granted_ones() {
        let s = site(ISSUER, None);
        let elsewhere = Buildit {
            site: &s,
            url: "http://host.openshell.internal:9000/mcp",
            host: "host.openshell.internal",
            port: 9000,
        };
        let args = elsewhere
            .args(
                &RunSite {
                    run_id: "r",
                    push_authfile: None,
                },
                "/p",
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["--bind", "0.0.0.0:9000"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--resource", "http://host.openshell.internal:9000/mcp"])
        );
    }

    #[test]
    fn an_issuer_without_a_key_set_uri_is_refused() {
        let s = site("https://crucible.hub.example", None);
        let run = RunSite {
            run_id: "r",
            push_authfile: None,
        };
        assert_eq!(
            buildit(&s).args(&run, "/p"),
            Err(BadIssuer("https://crucible.hub.example".into()))
        );
    }
}
