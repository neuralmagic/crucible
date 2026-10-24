use anyhow::Context;
use clap::{ArgAction, Parser};
use crucible_kueue::access::{SCOPE_READ, SCOPE_SUBMIT};
use crucible_kueue::caller::Manager;
use crucible_kueue::config::{
    self, ClaimMap, Config, Hardware, Labels, Lifetimes, Ownership, PullSecret, QueueVerb,
};
use crucible_kueue::profile::Profile;
use crucible_mcp_auth::policy::ToolPolicy;
use crucible_mcp_auth::server::{JwksTls, TrustedIssuer};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

/// Serve the crucible-kueue MCP endpoint.
#[derive(Parser)]
struct Cli {
    /// Listen address.
    #[arg(long, env = "CRUCIBLE_KUEUE_BIND", default_value = "0.0.0.0:8850")]
    bind: String,
    /// Kubeconfig context; unset uses in-cluster config or the current context.
    #[arg(long, env = "CRUCIBLE_KUEUE_CONTEXT")]
    context: Option<String>,
    /// Extra Host header values to accept besides loopback, e.g. the Service DNS name.
    #[arg(
        long = "allowed-host",
        env = "CRUCIBLE_KUEUE_ALLOWED_HOSTS",
        value_delimiter = ','
    )]
    allowed_hosts: Vec<String>,
    /// Accept runs' access tokens from an OAuth issuer (the crucible controller), as
    /// `<issuer>=<jwks uri>`. Repeatable.
    #[arg(
        long = "run-issuer",
        env = "CRUCIBLE_KUEUE_RUN_ISSUERS",
        value_delimiter = ',',
        value_parser = TrustedIssuer::runs,
        required_unless_present = "person_issuers"
    )]
    run_issuers: Vec<TrustedIssuer>,
    /// Accept people's access tokens from an OAuth issuer (the crucible controller, or another
    /// OIDC issuer), as `<issuer>=<jwks uri>`.
    /// Repeatable; needs --kube-authz.
    #[arg(
        long = "person-issuer",
        env = "CRUCIBLE_KUEUE_PERSON_ISSUERS",
        value_delimiter = ',',
        value_parser = TrustedIssuer::people,
        requires = "resources",
        requires = "kube_authz"
    )]
    person_issuers: Vec<TrustedIssuer>,
    /// The cluster this server acts on, as the issuer names it: a token's `kube_cluster` must
    /// name it. Unset, tokens for any cluster the issuer names are accepted.
    #[arg(long = "cluster", env = "CRUCIBLE_KUEUE_CLUSTER")]
    cluster: Option<String>,
    /// This server's canonical MCP URLs; an access token's aud must name one.
    #[arg(
        long = "resource",
        env = "CRUCIBLE_KUEUE_RESOURCES",
        value_delimiter = ',',
        required = true
    )]
    resources: Vec<url::Url>,
    /// PEM bundle of CAs to trust for issuers' JWKS hosts, beside the platform roots.
    #[arg(long, env = "CRUCIBLE_KUEUE_JWKS_CA")]
    jwks_ca: Option<PathBuf>,
    /// Allow plain-http JWKS URIs on non-loopback hosts. Only for a cluster network you trust.
    #[arg(long, env = "CRUCIBLE_KUEUE_INSECURE_JWKS_HTTP")]
    insecure_jwks_http: bool,
    /// TOML tool policy: CEL validate and mutate rules over tool arguments. Optional: the scopes
    /// and Kubernetes access each call needs are the server's own.
    #[arg(long, env = "CRUCIBLE_KUEUE_TOOL_POLICY")]
    tool_policy: Option<PathBuf>,
    /// Decide access checks for callers carrying kube_user with Kubernetes RBAC
    /// (SubjectAccessReview), taking the caller's groups from OpenShift Group objects or the
    /// token's kube_groups claim. Turns on people's tokens.
    #[arg(long, env = "CRUCIBLE_KUEUE_KUBE_AUTHZ", value_enum)]
    kube_authz: Option<KubeAuthz>,
    /// Domain of the labels and annotations the server stamps (`<domain>/run-id`, ...).
    #[arg(long, env = "CRUCIBLE_KUEUE_LABEL_DOMAIN", default_value = config::DEFAULT_LABEL_DOMAIN)]
    label_domain: String,
    /// The managed-by label's value and the field manager of every write. Give each server in a
    /// cluster its own, so each controls only its own objects.
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_MANAGER",
        default_value = config::DEFAULT_MANAGER,
        value_parser = |v: &str| Manager::parse(v)
    )]
    manager: Manager,
    /// managed: callers control only what this server created for them. rbac: any workload of a
    /// served kind their RBAC permits.
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_OWNERSHIP",
        value_enum,
        default_value = "managed"
    )]
    ownership: Ownership,
    /// Manifest allowlist: strict, tenant, or a TOML profile file.
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_ALLOWLIST_PROFILE",
        default_value = "strict"
    )]
    allowlist_profile: String,
    /// Claim naming the caller's Kubernetes user.
    #[arg(long, env = "CRUCIBLE_KUEUE_USER_CLAIM", default_value = "kube_user")]
    user_claim: String,
    /// Prefix for the user claim's value, matching the API server's --oidc-username-prefix.
    #[arg(long, env = "CRUCIBLE_KUEUE_USER_PREFIX", default_value = "")]
    user_prefix: String,
    /// Claim listing the caller's Kubernetes groups (with --kube-authz token).
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_GROUPS_CLAIM",
        default_value = "kube_groups"
    )]
    groups_claim: String,
    /// Claim naming a run in a run issuer's tokens; empty for none.
    #[arg(long, env = "CRUCIBLE_KUEUE_RUN_ID_CLAIM", default_value = "run_id")]
    run_id_claim: String,
    /// Claim naming the caller's default namespace.
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_NAMESPACE_CLAIM",
        default_value = "namespace"
    )]
    namespace_claim: String,
    /// LocalQueue verb submitting needs: Kueue's `use`, or `get` where roles do not grant it.
    #[arg(
        long,
        env = "CRUCIBLE_KUEUE_QUEUE_VERB",
        value_enum,
        default_value = "use"
    )]
    queue_verb: QueueVerb,
    /// activeDeadlineSeconds for a batch workload that sets none: seconds, or `none`.
    #[arg(long, env = "CRUCIBLE_KUEUE_DEFAULT_ACTIVE_DEADLINE", default_value = "86400", value_parser = seconds)]
    default_active_deadline: Seconds,
    /// ttlSecondsAfterFinished for a batch workload that sets none: seconds, or `none`.
    #[arg(long, env = "CRUCIBLE_KUEUE_DEFAULT_TTL_AFTER_FINISHED", default_value = "3600", value_parser = seconds)]
    default_ttl_after_finished: Seconds,
    /// Create Jobs, JobSets and PyTorchJobs suspended for Kueue to start.
    #[arg(long, env = "CRUCIBLE_KUEUE_SUSPEND", default_value_t = true, action = ArgAction::Set)]
    suspend: bool,
    /// Node resources that count as accelerators; a trailing `*` matches a prefix. Repeatable.
    #[arg(
        long = "accelerator-resource",
        env = "CRUCIBLE_KUEUE_ACCELERATOR_RESOURCES",
        value_delimiter = ','
    )]
    accelerator_resources: Vec<String>,
    /// Namespaces this server is installed in (its service account holds the workload role
    /// there). Empty discovers them by listing LocalQueues cluster-wide. Repeatable.
    #[arg(
        long = "namespace",
        env = "CRUCIBLE_KUEUE_NAMESPACES",
        value_delimiter = ','
    )]
    namespaces: Vec<String>,
    /// An image pull secret pod templates may name, as `name` or `name=registry` (a host or
    /// host/path prefix): the server adds it to any pod whose image is on that registry. With
    /// none, pod templates may name none. Repeatable.
    #[arg(
        long = "pull-secret",
        env = "CRUCIBLE_KUEUE_PULL_SECRETS",
        value_delimiter = ',',
        value_parser = PullSecret::parse
    )]
    pull_secrets: Vec<PullSecret>,
    /// Node labels naming an accelerator product, first match wins. Repeatable.
    #[arg(
        long = "accelerator-product-label",
        env = "CRUCIBLE_KUEUE_ACCELERATOR_PRODUCT_LABELS",
        value_delimiter = ','
    )]
    accelerator_product_labels: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "--run-id-claim \"\" names no run claim, so no run issuer's token could identify its run; \
     drop --run-issuer or name the claim"
)]
struct NoRunIdClaim;

/// A lifetime in seconds, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seconds(Option<i64>);

fn seconds(s: &str) -> Result<Seconds, String> {
    match s {
        "none" => Ok(Seconds(None)),
        n => n
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(|n| Seconds(Some(n)))
            .ok_or_else(|| format!("{s:?} is neither a number of seconds nor none")),
    }
}

impl Cli {
    fn config(&self) -> anyhow::Result<Config> {
        if self.run_id_claim.is_empty() && !self.run_issuers.is_empty() {
            return Err(NoRunIdClaim.into());
        }
        let defaults = Hardware::default();
        let or_default = |given: &[String], fallback: Vec<String>| match given.is_empty() {
            true => fallback,
            false => given.to_vec(),
        };
        Ok(Config {
            labels: Labels::new(&self.label_domain).managed_as(self.manager.clone()),
            ownership: self.ownership,
            profile: Profile::load(&self.allowlist_profile)?,
            claims: ClaimMap {
                user: self.user_claim.clone(),
                user_prefix: self.user_prefix.clone(),
                groups: self.groups_claim.clone(),
                run_id: (!self.run_id_claim.is_empty()).then(|| self.run_id_claim.clone()),
                namespace: self.namespace_claim.clone(),
            },
            queue_verb: self.queue_verb,
            lifetimes: Lifetimes {
                active_deadline_seconds: self.default_active_deadline.0,
                ttl_seconds_after_finished: self.default_ttl_after_finished.0,
                suspend: self.suspend,
            },
            hardware: Hardware {
                resources: or_default(&self.accelerator_resources, defaults.resources),
                product_labels: or_default(
                    &self.accelerator_product_labels,
                    defaults.product_labels,
                ),
            },
            namespaces: self.namespaces.clone(),
            pull_secrets: config::merge_pull_secrets(self.pull_secrets.clone()),
        })
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum KubeAuthz {
    Openshift,
    Token,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let cli = Cli::parse();
    let config = cli.config()?;
    crucible_kueue::install_tls_provider();
    let options = kube::config::KubeConfigOptions {
        context: cli.context.clone(),
        ..Default::default()
    };
    let kube_config = match &cli.context {
        Some(_) => kube::Config::from_kubeconfig(&options)
            .await
            .context("loading kubeconfig context")?,
        None => kube::Config::infer()
            .await
            .context("inferring kube config")?,
    };
    let client = kube::Client::try_from(kube_config).context("building kube client")?;
    let kueue = crucible_kueue::kueue::KueueVersion::discover(&client)
        .await
        .context("discovering the cluster's Kueue API version")?;
    tracing::info!(version = kueue.as_str(), "kueue api");
    let kinds = crucible_kueue::kinds::installed(&client)
        .await
        .context("discovering the served kinds the cluster installs")?;
    tracing::info!(
        kinds = ?kinds.iter().map(|k| k.kind()).collect::<Vec<_>>(),
        ownership = ?config.ownership,
        profile = %config.profile.name,
        label_domain = %config.labels.domain,
        "serving"
    );
    let issuers: Vec<TrustedIssuer> = cli
        .run_issuers
        .into_iter()
        .chain(cli.person_issuers)
        .collect();
    let names: Vec<(&str, _)> = issuers
        .iter()
        .map(|t| (t.issuer.as_str(), t.callers))
        .collect();
    tracing::info!(issuers = ?names, resources = ?cli.resources, "accepting access tokens");
    let rs_config = crucible_mcp_auth::server::ResourceServerConfig {
        issuers,
        resources: cli.resources,
        required_scopes: vec![SCOPE_READ.to_string()],
        scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_SUBMIT.to_string()],
        resource_name: Some("crucible-kueue".to_string()),
        cluster: cli.cluster,
    };
    let tls = JwksTls {
        ca_bundle: cli.jwks_ca,
        insecure_http: cli.insecure_jwks_http,
    };
    let oidc = Arc::new(
        crucible_mcp_auth::server::ResourceServer::new(rs_config, &tls)
            .context("building the resource server")?,
    );
    let policy = match &cli.tool_policy {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading tool policy {}", path.display()))?;
            let policy = ToolPolicy::from_toml(&text)
                .with_context(|| format!("loading tool policy {}", path.display()))?;
            tracing::info!(path = %path.display(), "tool policy loaded");
            policy
        }
        None => ToolPolicy::default(),
    };
    let reviewer: Option<Arc<dyn crucible_mcp_auth::access::AccessReview>> = match cli.kube_authz {
        Some(source) => {
            let groups = match source {
                KubeAuthz::Openshift => crucible_mcp_auth::kube::GroupSource::OpenShift,
                KubeAuthz::Token => crucible_mcp_auth::kube::GroupSource::Token,
            };
            tracing::info!(
                ?groups,
                "access checks decided by SubjectAccessReview; people's tokens accepted"
            );
            Some(Arc::new(
                crucible_mcp_auth::kube::SubjectAccessReviewer::new(client.clone(), groups),
            ))
        }
        None => None,
    };
    let app = crucible_kueue::router(
        client,
        crucible_kueue::Options {
            oidc,
            policy,
            reviewer,
            allowed_hosts: cli.allowed_hosts,
            kueue,
            kinds,
            config,
        },
    );
    let listener = tokio::net::TcpListener::bind(&cli.bind)
        .await
        .with_context(|| format!("binding {}", cli.bind))?;
    tracing::info!(bind = %cli.bind, path = crucible_kueue::MCP_PATH, "serving");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving http")
}

#[cfg(test)]
mod tests {
    use crate::*;
    use crucible_mcp_auth::principal::Callers;

    const RESOURCE: &str = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp";

    #[test]
    fn issuers_parse_as_issuer_equals_jwks_uri_and_the_flag_decides_their_callers() {
        let cli = Cli::try_parse_from([
            "crucible-kueue",
            "--run-issuer",
            "http://crucible-controller.crucible-system.svc:8080=http://crucible-controller.crucible-system.svc:8080/.well-known/jwks.json",
            "--person-issuer",
            "https://keycloak.example/realms/crucible=https://keycloak.auth.svc:8443/realms/crucible/protocol/openid-connect/certs?x=1",
            "--resource",
            RESOURCE,
            "--tool-policy",
            "policy.toml",
            "--kube-authz",
            "openshift",
        ])
        .unwrap();
        let shown = |issuers: &[TrustedIssuer]| {
            issuers
                .iter()
                .map(|t| (t.issuer.clone(), t.jwks_uri.to_string(), t.callers))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shown(&cli.run_issuers),
            [(
                "http://crucible-controller.crucible-system.svc:8080".to_string(),
                "http://crucible-controller.crucible-system.svc:8080/.well-known/jwks.json"
                    .to_string(),
                Callers::Runs
            )]
        );
        assert_eq!(
            shown(&cli.person_issuers),
            [(
                "https://keycloak.example/realms/crucible".to_string(),
                "https://keycloak.auth.svc:8443/realms/crucible/protocol/openid-connect/certs?x=1"
                    .to_string(),
                Callers::People
            )]
        );
    }

    #[test]
    fn no_new_flags_is_crucibles_config() {
        let cli = Cli::try_parse_from([
            "crucible-kueue",
            "--run-issuer",
            "https://a.example=https://a.example/keys",
            "--resource",
            RESOURCE,
        ])
        .unwrap();
        let got = cli.config().unwrap();
        let want = Config::default();
        assert_eq!(got.labels, want.labels);
        assert_eq!(got.ownership, want.ownership);
        assert_eq!(got.profile, want.profile);
        assert_eq!(got.claims, want.claims);
        assert_eq!(got.queue_verb, want.queue_verb);
        assert_eq!(got.lifetimes, want.lifetimes);
        assert_eq!(got.hardware, want.hardware);
        assert_eq!(got.namespaces, want.namespaces);
        assert_eq!(got.pull_secrets, want.pull_secrets);
    }

    #[test]
    fn the_manager_names_labels_and_writes_and_must_be_a_dns_label() {
        let parse = |manager: &str| {
            Cli::try_parse_from([
                "crucible-kueue",
                "--run-issuer",
                "https://a.example=https://a.example/keys",
                "--resource",
                RESOURCE,
                "--manager",
                manager,
            ])
        };
        let config = parse("team-b-kueue").unwrap().config().unwrap();
        assert_eq!(config.labels.manager.as_str(), "team-b-kueue");
        assert_eq!(config.labels.managed_by, "crucible.dev/managed-by");
        for bad in ["", "Team_B", "a/b"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn run_issuers_need_a_run_claim() {
        let cli = Cli::try_parse_from([
            "crucible-kueue",
            "--run-issuer",
            "https://a.example=https://a.example/keys",
            "--resource",
            RESOURCE,
            "--run-id-claim",
            "",
        ])
        .unwrap();
        let error = cli.config().err().unwrap();
        assert!(error.downcast_ref::<NoRunIdClaim>().is_some(), "{error}");
    }

    #[test]
    fn a_generic_deployment_is_configured_through_flags() {
        let cli = Cli::try_parse_from([
            "crucible-kueue",
            "--person-issuer",
            "https://idp.example/realms/r=https://idp.example/realms/r/certs",
            "--resource",
            RESOURCE,
            "--kube-authz",
            "token",
            "--label-domain",
            "kueue-mcp.example.com",
            "--ownership",
            "rbac",
            "--allowlist-profile",
            "tenant",
            "--user-claim",
            "preferred_username",
            "--user-prefix",
            "oidc:",
            "--groups-claim",
            "groups",
            "--run-id-claim",
            "",
            "--queue-verb",
            "get",
            "--default-active-deadline",
            "none",
            "--default-ttl-after-finished",
            "600",
            "--suspend",
            "false",
            "--accelerator-resource",
            "nvidia.com/gpu,example.com/npu",
            "--namespace",
            "ml-a,ml-b",
            "--pull-secret",
            "quay-pull=quay.io/example,quay-pull=quay.io/crucible,public-only",
        ])
        .unwrap();
        let c = cli.config().unwrap();
        assert_eq!(c.labels.run_id, "kueue-mcp.example.com/run-id");
        assert_eq!(c.ownership, Ownership::Rbac);
        assert_eq!(c.profile, Profile::tenant());
        assert_eq!(c.claims.user, "preferred_username");
        assert_eq!(c.claims.user_prefix, "oidc:");
        assert_eq!(c.claims.groups, "groups");
        assert_eq!(c.claims.run_id, None);
        assert_eq!(c.queue_verb, QueueVerb::Get);
        assert_eq!(
            c.lifetimes,
            Lifetimes {
                active_deadline_seconds: None,
                ttl_seconds_after_finished: Some(600),
                suspend: false,
            }
        );
        assert_eq!(c.hardware.resources, ["nvidia.com/gpu", "example.com/npu"]);
        assert_eq!(c.namespaces, ["ml-a", "ml-b"]);
        assert_eq!(
            c.pull_secrets,
            [
                PullSecret {
                    name: "quay-pull".into(),
                    registries: vec!["quay.io/example".into(), "quay.io/crucible".into()],
                },
                PullSecret {
                    name: "public-only".into(),
                    registries: vec![],
                },
            ]
        );
        assert_eq!(
            c.hardware.product_labels,
            Hardware::default().product_labels
        );
        assert!(seconds("-1").is_err() && seconds("soon").is_err());
    }

    #[test]
    fn an_issuer_and_resources_are_required_and_people_need_kube_authz() {
        let refused = [
            vec![],
            vec!["--resource", RESOURCE],
            vec!["--run-issuer", "https://a.example=https://a.example/keys"],
            vec![
                "--person-issuer",
                "https://a.example=https://a.example/keys",
                "--resource",
                RESOURCE,
            ],
        ];
        for args in refused {
            assert!(
                Cli::try_parse_from(std::iter::once("crucible-kueue").chain(args.iter().copied()))
                    .is_err(),
                "{args:?}"
            );
        }
        Cli::try_parse_from([
            "crucible-kueue",
            "--run-issuer",
            "https://a.example=https://a.example/keys",
            "--resource",
            RESOURCE,
        ])
        .unwrap();
    }
}
