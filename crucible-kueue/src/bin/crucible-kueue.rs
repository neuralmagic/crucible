use anyhow::Context;
use clap::Parser;
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
    /// Accept people's access tokens from an OAuth issuer (the realm), as `<issuer>=<jwks uri>`.
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
    crucible_kueue::install_tls_provider();
    let options = kube::config::KubeConfigOptions {
        context: cli.context.clone(),
        ..Default::default()
    };
    let config = match &cli.context {
        Some(_) => kube::Config::from_kubeconfig(&options)
            .await
            .context("loading kubeconfig context")?,
        None => kube::Config::infer()
            .await
            .context("inferring kube config")?,
    };
    let client = kube::Client::try_from(config).context("building kube client")?;
    let kueue = crucible_kueue::kueue::KueueVersion::discover(&client)
        .await
        .context("discovering the cluster's Kueue API version")?;
    tracing::info!(version = kueue.as_str(), "kueue api");
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
    let config = crucible_mcp_auth::server::ResourceServerConfig {
        issuers,
        resources: cli.resources,
        required_scopes: vec![crucible_contract::kueue::SCOPE_READ.to_string()],
        scopes_supported: vec![
            crucible_contract::kueue::SCOPE_READ.to_string(),
            crucible_contract::kueue::SCOPE_SUBMIT.to_string(),
        ],
        resource_name: Some("crucible-kueue".to_string()),
    };
    let tls = JwksTls {
        ca_bundle: cli.jwks_ca,
        insecure_http: cli.insecure_jwks_http,
    };
    let oidc = Arc::new(
        crucible_mcp_auth::server::ResourceServer::new(config, &tls)
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
