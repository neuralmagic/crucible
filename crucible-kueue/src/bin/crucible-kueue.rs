use anyhow::Context;
use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

/// Serve the crucible-kueue MCP endpoint.
#[derive(Parser)]
struct Cli {
    /// Listen address.
    #[arg(long, env = "CRUCIBLE_KUEUE_BIND", default_value = "0.0.0.0:8850")]
    bind: String,
    /// TOML file of static tenants (token digest, namespace, role, queues).
    #[arg(long, env = "CRUCIBLE_KUEUE_TENANTS")]
    tenants: Option<PathBuf>,
    /// Namespace whose labelled ConfigMaps hold controller-written tenant records. Only the
    /// controller may write ConfigMaps there.
    #[arg(long, env = "CRUCIBLE_KUEUE_TENANT_NAMESPACE")]
    tenant_namespace: Option<String>,
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
    /// Accept access tokens from this OAuth issuer (the crucible realm), alongside tenant tokens.
    #[arg(long, env = "CRUCIBLE_KUEUE_OIDC_ISSUER", requires_all = ["oidc_jwks_uri", "resources"])]
    oidc_issuer: Option<url::Url>,
    /// The issuer's JWKS endpoint, e.g. the in-cluster Keycloak Service.
    #[arg(long, env = "CRUCIBLE_KUEUE_OIDC_JWKS_URI", requires = "oidc_issuer")]
    oidc_jwks_uri: Option<url::Url>,
    /// This server's canonical MCP URLs; an access token's aud must name one.
    #[arg(
        long = "resource",
        env = "CRUCIBLE_KUEUE_RESOURCES",
        value_delimiter = ',',
        requires = "oidc_issuer"
    )]
    resources: Vec<url::Url>,
    /// TOML tool policy: per-tool scopes, CEL validate rules, and claim-derived argument mutations.
    #[arg(long, env = "CRUCIBLE_KUEUE_TOOL_POLICY")]
    tool_policy: Option<PathBuf>,
    /// Read a token from stdin, print its SHA-256 digest for the tenants file, and exit.
    #[arg(long, exclusive = true)]
    digest: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("no callers: pass --tenants with at least one entry, --tenant-namespace, or --oidc-issuer")]
struct NoTenants;

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
    if cli.digest {
        let mut token = String::new();
        std::io::stdin()
            .read_line(&mut token)
            .context("reading the token from stdin")?;
        println!(
            "{}",
            crucible_contract::kueue::token_digest(token.trim_end())
        );
        return Ok(());
    }
    let tenants = match &cli.tenants {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading tenants file {}", path.display()))?;
            crucible_kueue::tenant::Tenants::from_toml(&text).context("loading tenants")?
        }
        None => crucible_kueue::tenant::Tenants::default(),
    };
    if tenants.is_empty() && cli.tenant_namespace.is_none() && cli.oidc_issuer.is_none() {
        return Err(NoTenants.into());
    }
    tracing::info!(tenants = tenants.len(), "static tenants loaded");
    let registry = Arc::new(crucible_kueue::tenant::Registry::new(tenants));
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
    if let Some(namespace) = cli.tenant_namespace.clone() {
        tracing::info!(namespace, "watching tenant records");
        tokio::spawn(crucible_kueue::records::watch(
            client.clone(),
            namespace,
            registry.clone(),
        ));
    }
    let oidc = match (cli.oidc_issuer, cli.oidc_jwks_uri) {
        (Some(issuer), Some(jwks_uri)) => {
            tracing::info!(%issuer, resources = ?cli.resources, "accepting realm access tokens");
            let config = crucible_mcp_auth::server::ResourceServerConfig {
                issuer,
                jwks_uri,
                resources: cli.resources,
                required_scopes: vec![crucible_kueue::auth::SCOPE_READ.to_string()],
                scopes_supported: vec![
                    crucible_kueue::auth::SCOPE_READ.to_string(),
                    crucible_kueue::auth::SCOPE_SUBMIT.to_string(),
                ],
                resource_name: Some("crucible-kueue".to_string()),
            };
            let server = crucible_mcp_auth::server::ResourceServer::new(config)
                .context("building the JWKS client")?;
            Some(Arc::new(server))
        }
        _ => None,
    };
    let policy = match &cli.tool_policy {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading tool policy {}", path.display()))?;
            let policy = crucible_mcp_auth::policy::ToolPolicy::from_toml(&text)
                .with_context(|| format!("loading tool policy {}", path.display()))?;
            tracing::info!(path = %path.display(), "tool policy loaded");
            Some(policy)
        }
        None => None,
    };
    let auth = crucible_kueue::auth::Auth { registry, oidc };
    let app = crucible_kueue::router(client, auth, policy, &cli.allowed_hosts);
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
