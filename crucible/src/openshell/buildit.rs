//! The run's buildit, started in the loop pod when the controller granted the run a server served
//! there by `buildit mcp`: it runs builder pods in the profile's buildit namespace from the run's
//! own sandboxes, through the run's gateway, and takes only run tokens the controller issued for
//! this run. Its grant, egress and `.mcp.json` entry are any agent server's
//! ([`crate::openshell::agent_servers`]).

use anyhow::{Context, Result};
use crucible_contract::agent_servers::{BuilditSite, NodeArch};
use std::path::{Path, PathBuf};
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

/// Where the run's own command and evaluate tasks reach its buildit: a unix socket in the run's
/// state dir, whose builds read their contexts from the run's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSocket {
    pub path: PathBuf,
    pub workdir: PathBuf,
}

impl RunSocket {
    pub(crate) fn of(paths: &crate::args::Paths) -> Self {
        Self {
            path: paths.state.join("buildit.sock"),
            workdir: paths.workspace.clone(),
        }
    }
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
    pub socket: Option<&'a RunSocket>,
}

/// What `buildit mcp` runs as for this run.
#[derive(Debug, PartialEq, Eq)]
pub struct RunSite<'a> {
    /// The run pod's name, the `run_id` claim of this run's tokens.
    pub run_id: &'a str,
    pub push_authfile: Option<&'a str>,
    /// The loop pod's architecture, where builder pods run unless the site names one.
    pub arch: Option<NodeArch>,
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
        if let Some(queue) = &self.site.queue {
            args.extend(["--queue", queue].map(String::from));
        }
        if let (Some(repo), Some(authfile)) = (&self.site.push_repo, run.push_authfile) {
            args.extend(["--push-repo", repo, "--push-authfile", authfile].map(String::from));
        }
        let pods = &self.site.builders;
        if let Some(arch) = pods.node_arch.or(run.arch) {
            args.extend(["--node-arch".to_string(), arch.to_string()]);
        }
        if let Some(n) = pods.max_builds {
            args.extend(["--max-builds".to_string(), n.to_string()]);
        }
        if let Some(secs) = pods.pod_deadline_secs {
            args.extend(["--pod-deadline".to_string(), secs.to_string()]);
        }
        for (flag, resources) in [("--request", &pods.requests), ("--limit", &pods.limits)] {
            for (key, qty) in resources.iter().flatten() {
                args.extend([flag.to_string(), format!("{key}={qty}")]);
            }
        }
        if let Some(socket) = self.socket {
            for (flag, path) in [
                ("--socket", &socket.path),
                ("--socket-workdir", &socket.workdir),
            ] {
                args.extend([flag.to_string(), path.to_string_lossy().into_owned()]);
            }
        }
        Ok(args)
    }

    fn listening(&self, probe: &str) -> bool {
        crate::control::broker::port_open(probe) && self.socket.is_none_or(|s| accepts(&s.path))
    }

    /// Start buildit for this run unless it is already listening, and wait until it accepts
    /// connections. It is the loop pod's child for the run's lifetime, like the broker.
    pub fn ensure_running(&self) -> Result<()> {
        let probe = format!("127.0.0.1:{}", self.port);
        if self.listening(&probe) {
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
            arch: NodeArch::host(),
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
            if self.listening(&probe) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let probe = match self.socket {
            Some(socket) => format!("{probe} and {}", socket.path.display()),
            None => probe,
        };
        Err(BootTimeout {
            probe,
            seconds: BOOT_TIMEOUT.as_secs(),
        }
        .into())
    }
}

fn accepts(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(test)]
mod tests {
    use crate::openshell::buildit::*;
    use crucible_contract::agent_servers::BuilderPods;
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    const ISSUER: &str =
        "https://crucible.hub.example=https://crucible-ext.hub.example/.well-known/jwks.json";
    const URL: &str = "http://host.openshell.internal:8848/mcp";

    fn site(issuer: &str, push: Option<&str>) -> BuilditSite {
        BuilditSite {
            run_issuer: issuer.into(),
            namespace: "crucible-builds".into(),
            push_repo: push.map(str::to_string),
            queue: None,
            builders: BuilderPods::default(),
        }
    }

    fn buildit(site: &BuilditSite) -> Buildit<'_> {
        Buildit {
            site,
            url: URL,
            host: "host.openshell.internal",
            port: 8848,
            socket: None,
        }
    }

    #[test]
    fn buildit_serves_this_run_alone_and_pushes_only_with_a_repo_and_an_authfile() {
        let run = RunSite {
            run_id: "crucible-run-brave-otter",
            push_authfile: Some("/etc/quay/push.json"),
            arch: None,
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
        assert!(!args.contains(&"--queue".to_string()), "no queue: {args:?}");
        let queued = BuilditSite {
            queue: Some("builders".into()),
            ..plain.clone()
        };
        assert_eq!(
            flag(&buildit(&queued).args(&run, "/p").unwrap(), "--queue").as_deref(),
            Some("builders")
        );

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
            socket: None,
        };
        let args = elsewhere
            .args(
                &RunSite {
                    run_id: "r",
                    push_authfile: None,
                    arch: None,
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
    fn the_runs_own_socket_sits_in_its_state_dir_and_builds_from_its_workspace() {
        let dir = std::path::Path::new("/runs/r1/pack");
        let paths = crate::args::Paths::for_manifest(dir.join("ws"), dir.join("state"), dir, None);
        let socket = RunSocket::of(&paths);
        assert_eq!(socket.path, dir.join("state/buildit.sock"));
        assert_eq!(socket.workdir, dir.join("ws"));
        let s = site(ISSUER, None);
        let run = RunSite {
            run_id: "r",
            push_authfile: None,
            arch: None,
        };
        let served = Buildit {
            socket: Some(&socket),
            ..buildit(&s)
        };
        let args = served.args(&run, "/p").unwrap();
        assert!(
            args.windows(2)
                .any(|w| w == ["--socket", "/runs/r1/pack/state/buildit.sock"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--socket-workdir", "/runs/r1/pack/ws"])
        );
        let args = buildit(&s).args(&run, "/p").unwrap();
        assert!(!args.iter().any(|a| a.starts_with("--socket")), "{args:?}");
    }

    #[test]
    fn only_a_socket_something_listens_on_counts() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("buildit.sock");
        assert!(!accepts(&socket));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(accepts(&socket));
        drop(listener);
        assert!(
            socket.exists() && !accepts(&socket),
            "a dead server's socket"
        );
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(!accepts(&file));
    }

    #[test]
    fn an_issuer_without_a_key_set_uri_is_refused() {
        let s = site("https://crucible.hub.example", None);
        let run = RunSite {
            run_id: "r",
            push_authfile: None,
            arch: None,
        };
        assert_eq!(
            buildit(&s).args(&run, "/p"),
            Err(BadIssuer("https://crucible.hub.example".into()))
        );
    }

    fn builder_flags(args: &[String]) -> Vec<&str> {
        let at = args
            .iter()
            .position(|a| a == "--namespace")
            .map_or(args.len(), |i| i + 2);
        args[at..]
            .iter()
            .map(String::as_str)
            .filter(|a| *a != "--insecure-jwks-http")
            .collect()
    }

    #[test]
    fn builder_pods_run_on_the_site_s_arch_with_its_limits() {
        let mut s = site(ISSUER, None);
        s.builders = BuilderPods {
            node_arch: Some(NodeArch::Amd64),
            max_builds: NonZeroU32::new(2),
            pod_deadline_secs: NonZeroU32::new(3600),
            requests: Some(BTreeMap::from([
                ("cpu".into(), "8".into()),
                ("memory".into(), "16Gi".into()),
            ])),
            limits: Some(BTreeMap::from([("cpu".into(), "32".into())])),
        };
        let on_arm = RunSite {
            run_id: "r",
            push_authfile: None,
            arch: Some(NodeArch::Arm64),
        };
        let args = buildit(&s).args(&on_arm, "/p").unwrap();
        assert_eq!(
            builder_flags(&args),
            [
                "--node-arch",
                "amd64",
                "--max-builds",
                "2",
                "--pod-deadline",
                "3600",
                "--request",
                "cpu=8",
                "--request",
                "memory=16Gi",
                "--limit",
                "cpu=32",
            ],
            "the site's arch wins over the loop pod's"
        );
    }

    #[test]
    fn builder_pods_default_to_the_loop_pod_s_arch_and_buildit_s_limits() {
        let s = site(ISSUER, None);
        let run = |arch| RunSite {
            run_id: "r",
            push_authfile: None,
            arch,
        };
        let args = buildit(&s).args(&run(Some(NodeArch::Arm64)), "/p").unwrap();
        assert_eq!(builder_flags(&args), ["--node-arch", "arm64"]);
        let args = buildit(&s).args(&run(None), "/p").unwrap();
        assert!(builder_flags(&args).is_empty(), "{args:?}");

        let mut unbounded = site(ISSUER, None);
        unbounded.builders.requests = Some(BTreeMap::new());
        let args = buildit(&unbounded).args(&run(None), "/p").unwrap();
        assert!(builder_flags(&args).is_empty(), "{args:?}");
    }
}
