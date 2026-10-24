//! The run's buildit, started in the loop pod when the controller gave the run buildit access:
//! it runs builder pods in the profile's buildit namespace from the run's own sandboxes, through the run's gateway, and
//! takes only run tokens the controller issued for this run. The sandbox's supervisor obtains
//! each token and injects it on traffic to buildit alone.

use crate::openshell::provider;
use anyhow::{Context, Result};
use crucible_contract::buildit::{
    ENV_BUILDIT_NAMESPACE, ENV_BUILDIT_PUSH_REPO, ENV_BUILDIT_RUN_ISSUER, PORT, SCOPE_BUILD,
    SCOPE_READ, URL,
};
use crucible_contract::sandbox::{ENV_AGENT_SVID_AUDIENCE, ENV_AGENT_TOKEN_ENDPOINT};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The `.mcp.json` server name the agent sees buildit's tools under.
pub const MCP_SERVER_NAME: &str = "buildit";

/// The name sandboxes reach the loop pod by.
const HOST: &str = "host.openshell.internal";

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
pub enum BuilditAccessError {
    #[error("{ENV_BUILDIT_RUN_ISSUER} is set but {missing} is not; the controller sets both")]
    Partial { missing: &'static str },
    #[error("{set} is set but {ENV_BUILDIT_RUN_ISSUER} is not; the controller sets both")]
    NoIssuer { set: &'static str },
    #[error("{ENV_BUILDIT_RUN_ISSUER} {0:?} is not <issuer>=<jwks uri>")]
    Issuer(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct BuilditAccess {
    /// `<issuer>=<jwks uri>`, buildit's `--run-issuer`.
    trusted_issuer: String,
    jwks_over_http: bool,
    /// Where builder pods go.
    namespace: String,
    push_repo: Option<String>,
    endpoint: String,
    svid_audience: String,
}

/// What `buildit mcp` runs as for this run.
#[derive(Debug, PartialEq, Eq)]
pub struct RunSite<'a> {
    /// The run pod's name, the `run_id` claim of this run's tokens.
    pub run_id: &'a str,
    pub push_authfile: Option<&'a str>,
}

impl BuilditAccess {
    /// The access from the loop pod's environment; `None` when this run has none.
    pub fn from_env() -> Result<Option<Self>, BuilditAccessError> {
        let var = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Self::from_values(
            var(ENV_BUILDIT_RUN_ISSUER),
            var(ENV_BUILDIT_NAMESPACE),
            var(ENV_BUILDIT_PUSH_REPO),
            var(ENV_AGENT_TOKEN_ENDPOINT),
            var(ENV_AGENT_SVID_AUDIENCE),
        )
    }

    fn from_values(
        issuer: Option<String>,
        namespace: Option<String>,
        push_repo: Option<String>,
        endpoint: Option<String>,
        svid_audience: Option<String>,
    ) -> Result<Option<Self>, BuilditAccessError> {
        let Some(trusted_issuer) = issuer else {
            return match (&namespace, &push_repo) {
                (Some(_), _) => Err(BuilditAccessError::NoIssuer {
                    set: ENV_BUILDIT_NAMESPACE,
                }),
                (None, Some(_)) => Err(BuilditAccessError::NoIssuer {
                    set: ENV_BUILDIT_PUSH_REPO,
                }),
                (None, None) => Ok(None),
            };
        };
        let namespace = namespace.ok_or(BuilditAccessError::Partial {
            missing: ENV_BUILDIT_NAMESPACE,
        })?;
        let endpoint = endpoint.ok_or(BuilditAccessError::Partial {
            missing: ENV_AGENT_TOKEN_ENDPOINT,
        })?;
        let svid_audience = svid_audience.ok_or(BuilditAccessError::Partial {
            missing: ENV_AGENT_SVID_AUDIENCE,
        })?;
        let jwks = trusted_issuer
            .split_once('=')
            .and_then(|(_, jwks)| url::Url::parse(jwks).ok())
            .ok_or_else(|| BuilditAccessError::Issuer(trusted_issuer.clone()))?;
        Ok(Some(Self {
            jwks_over_http: jwks.scheme() == "http",
            trusted_issuer,
            namespace,
            push_repo,
            endpoint,
            svid_audience,
        }))
    }

    pub fn url(&self) -> &'static str {
        URL
    }

    /// The sandbox egress entry: HTTP-inspected, so the supervisor injects the token.
    pub fn egress_endpoint(&self) -> String {
        format!("{HOST}:{PORT}:full:rest")
    }

    /// The token-grant provider profile; its endpoint binds the injected token to buildit alone.
    pub fn token_grant_profile(&self) -> openshell_core::proto::ProviderProfile {
        provider::token_grant_profile(&provider::GrantedServer {
            profile_id: provider::BUILDIT_GRANT_PROFILE_ID,
            name: "buildit",
            host: HOST,
            port: PORT,
            url: URL,
            token_endpoint: &self.endpoint,
            svid_audience: &self.svid_audience,
            scopes: &[SCOPE_READ, SCOPE_BUILD],
        })
    }

    /// `buildit mcp`'s arguments for `site`, reading its tool policy from `policy`.
    pub fn args(&self, site: &RunSite<'_>, policy: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "mcp",
            "--bind",
            &format!("0.0.0.0:{PORT}"),
            "--run-issuer",
            &self.trusted_issuer,
            "--resource",
            URL,
            "--allowed-host",
            HOST,
            "--tool-policy",
            policy,
            "--run-id",
            site.run_id,
            "--namespace",
            &self.namespace,
        ]
        .map(String::from)
        .to_vec();
        if self.jwks_over_http {
            args.push("--insecure-jwks-http".to_string());
        }
        if let (Some(repo), Some(authfile)) = (&self.push_repo, site.push_authfile) {
            args.extend(["--push-repo", repo, "--push-authfile", authfile].map(String::from));
        }
        args
    }

    /// Start buildit for this run unless it is already listening, and wait until it accepts
    /// connections. It is the loop pod's child for the run's lifetime, like the broker.
    pub fn ensure_running(&self) -> Result<()> {
        let probe = format!("127.0.0.1:{PORT}");
        if crate::control::broker::port_open(&probe) {
            return Ok(());
        }
        let run_id = std::env::var(crucible_contract::ENV_POD_NAME)
            .context("buildit needs the run pod's name to know its run")?;
        let push_authfile = std::env::var(ENV_PUSH_AUTHFILE).ok();
        if self.push_repo.is_some() && push_authfile.is_none() {
            tracing::warn!(
                "{ENV_BUILDIT_PUSH_REPO} is set but the loop pod has no push authfile; buildit pushes nothing"
            );
        }
        let policy = std::env::temp_dir().join("buildit-policy.toml");
        std::fs::write(&policy, POLICY).with_context(|| format!("writing {}", policy.display()))?;
        let site = RunSite {
            run_id: &run_id,
            push_authfile: push_authfile.as_deref(),
        };
        Command::new("buildit")
            .args(self.args(&site, &policy.to_string_lossy()))
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
    const ENDPOINT: &str = "https://crucible-ext.hub.example/oauth/token";
    const AUDIENCE: &str = "https://crucible.hub.example";

    fn access(push: Option<&str>) -> BuilditAccess {
        BuilditAccess::from_values(
            Some(ISSUER.into()),
            Some("crucible-builds".into()),
            push.map(str::to_string),
            Some(ENDPOINT.into()),
            Some(AUDIENCE.into()),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn no_issuer_means_no_buildit_and_an_issuer_needs_the_token_endpoint() {
        assert!(
            BuilditAccess::from_values(
                None,
                None,
                None,
                Some(ENDPOINT.into()),
                Some(AUDIENCE.into())
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            BuilditAccess::from_values(
                Some(ISSUER.into()),
                Some("crucible-builds".into()),
                None,
                None,
                Some(AUDIENCE.into())
            ),
            Err(BuilditAccessError::Partial {
                missing: ENV_AGENT_TOKEN_ENDPOINT
            })
        );
        assert_eq!(
            BuilditAccess::from_values(
                Some(ISSUER.into()),
                None,
                None,
                Some(ENDPOINT.into()),
                Some(AUDIENCE.into())
            ),
            Err(BuilditAccessError::Partial {
                missing: ENV_BUILDIT_NAMESPACE
            }),
            "builder pods never fall back to the loop namespace"
        );
        assert!(matches!(
            BuilditAccess::from_values(
                Some("https://crucible.hub.example".into()),
                Some("crucible-builds".into()),
                None,
                Some(ENDPOINT.into()),
                Some(AUDIENCE.into())
            ),
            Err(BuilditAccessError::Issuer(_))
        ));
    }

    #[test]
    fn buildit_settings_without_an_issuer_are_refused_not_ignored() {
        assert_eq!(
            BuilditAccess::from_values(
                None,
                None,
                Some("quay.io/team/agent".into()),
                Some(ENDPOINT.into()),
                Some(AUDIENCE.into())
            ),
            Err(BuilditAccessError::NoIssuer {
                set: ENV_BUILDIT_PUSH_REPO
            })
        );
        assert_eq!(
            BuilditAccess::from_values(None, Some("crucible-builds".into()), None, None, None),
            Err(BuilditAccessError::NoIssuer {
                set: ENV_BUILDIT_NAMESPACE
            })
        );
    }

    #[test]
    fn the_token_grant_is_bound_to_buildit_in_the_loop_pod_with_buildits_scopes() {
        let a = access(None);
        assert_eq!(
            a.egress_endpoint(),
            "host.openshell.internal:8848:full:rest"
        );
        let profile = a.token_grant_profile();
        assert_eq!(profile.id, provider::BUILDIT_GRANT_PROFILE_ID);
        let endpoint = &profile.endpoints[0];
        assert_eq!(
            (
                endpoint.host.as_str(),
                endpoint.port,
                endpoint.protocol.as_str()
            ),
            ("host.openshell.internal", 8848, "rest")
        );
        let grant = profile.credentials[0].token_grant.as_ref().unwrap();
        assert_eq!(grant.audience, "http://host.openshell.internal:8848/mcp");
        assert_eq!(grant.token_endpoint, ENDPOINT);
        assert_eq!(grant.jwt_svid_audience, AUDIENCE);
        assert_eq!(grant.scopes, ["buildit.read", "buildit.build"]);
        assert!(profile.credentials[0].env_vars.is_empty(), "no placeholder");
    }

    #[test]
    fn buildit_serves_this_run_alone_and_pushes_only_with_a_repo_and_an_authfile() {
        let site = RunSite {
            run_id: "crucible-run-brave-otter",
            push_authfile: Some("/etc/quay/push.json"),
        };
        let args = access(None).args(&site, "/tmp/policy.toml");
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
        assert_eq!(
            flag(&args, "--resource").as_deref(),
            Some("http://host.openshell.internal:8848/mcp")
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

        let pushing = access(Some("quay.io/example/crucible-smoke")).args(&site, "/p");
        assert_eq!(
            flag(&pushing, "--push-repo").as_deref(),
            Some("quay.io/example/crucible-smoke")
        );
        assert_eq!(
            flag(&pushing, "--push-authfile").as_deref(),
            Some("/etc/quay/push.json")
        );
        let no_authfile = RunSite {
            push_authfile: None,
            ..site
        };
        assert!(
            !access(Some("quay.io/example/crucible-smoke"))
                .args(&no_authfile, "/p")
                .contains(&"--push-repo".to_string())
        );

        let plain = BuilditAccess::from_values(
            Some("http://ctl.ns.svc:8080=http://ctl.ns.svc:8080/.well-known/jwks.json".into()),
            Some("crucible-builds".into()),
            None,
            Some(ENDPOINT.into()),
            Some(AUDIENCE.into()),
        )
        .unwrap()
        .unwrap();
        assert!(
            plain
                .args(&no_authfile, "/p")
                .contains(&"--insecure-jwks-http".to_string())
        );
    }
}
