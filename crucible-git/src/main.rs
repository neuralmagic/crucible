//! `crucible-git serve`: one run's git and GitHub calls, made with the run's minted GitHub App
//! token, which it reads from its file on every request so a token refreshed in place is used at
//! once. It runs in a container of its own, the only one the token is mounted into, and reaches
//! only the repositories the run's `[outputs.draft-pr]` bound admits.
//!
//! ```text
//!   sandbox ──run token──▶ --bind      ─┐  /git/<owner>/<repo>.git/…  ─▶ github.com
//!           ──run token──▶ --bind      ─┤  /mcp  open_pr              ─▶ api.github.com
//!   task    ──TLS (SNI)───▶ --tls-bind ─┘  plain gh and git (HTTPS)   ─▶ GitHub
//! ```

mod allow;
mod github;
mod graphql;
mod mcp;
mod mitm;
mod policy;
mod proxy;
mod relay;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use clap::{Args, Parser, Subcommand};
use crucible_mcp_auth::principal::Callers;
use crucible_mcp_auth::server::{JwksTls, ResourceServer, ResourceServerConfig, TrustedIssuer};
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

const SCOPE_READ: &str = "git.read";
const SCOPE_WRITE: &str = "git.write";

#[derive(Parser)]
#[command(name = "crucible-git", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve one run: sandboxes on `--bind` with run tokens, the run's tasks on `--local-bind`.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Where sandboxes reach the server.
    #[arg(long)]
    bind: String,
    /// An issuer of run tokens, `<issuer>=<jwks uri>`.
    #[arg(long = "run-issuer", value_parser = run_issuer, required = true)]
    run_issuers: Vec<TrustedIssuer>,
    /// The URL sandboxes reach the server at, a token's `aud`.
    #[arg(long = "resource", required = true)]
    resources: Vec<Url>,
    /// A `Host` sandboxes reach the server by, beside loopback.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// The run this server serves, compared with a run token's `run_id`.
    #[arg(long)]
    run_id: String,
    /// The file holding the run's GitHub App token. Unset, every call is refused.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// The run's output bounds, as the loop writes them at setup.
    #[arg(long)]
    bounds_file: PathBuf,
    /// Where GitHub's hostnames are served over TLS to the run's tasks: the loopback address the
    /// pod maps them to.
    #[arg(long)]
    tls_bind: String,
    /// Where to write the run CA and the bundle tasks trust.
    #[arg(long)]
    ca_dir: PathBuf,
    #[arg(long, default_value = "https://api.github.com")]
    api_url: Url,
    /// PEM bundle of CAs trusted for the issuer's JWKS host.
    #[arg(long)]
    jwks_ca: Option<PathBuf>,
}

fn run_issuer(arg: &str) -> Result<TrustedIssuer, String> {
    TrustedIssuer::runs(arg).map_err(|e| e.to_string())
}

/// How sandbox requests are authenticated: a run token from a trusted issuer, for this run.
struct Sandbox {
    server: Arc<ResourceServer>,
    run_id: String,
}

async fn authenticate(
    State(sandbox): State<Arc<Sandbox>>,
    mut req: Request,
    next: Next,
) -> Response {
    let principal = match sandbox.server.authenticate(req.headers()).await {
        Ok(p) => p,
        Err(e) => return sandbox.server.challenge(req.headers(), &e),
    };
    let ours = principal.callers == Callers::Runs
        && principal.claims.get("run_id").and_then(|v| v.as_str()) == Some(&sandbox.run_id);
    if !ours {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "the token is not for this run",
        )
            .into_response();
    }
    req.extensions_mut().insert(principal);
    next.run(req).await
}

async fn serve(args: ServeArgs) -> Result<()> {
    let server = Arc::new(
        ResourceServer::new(
            ResourceServerConfig {
                issuers: args.run_issuers,
                resources: args.resources,
                required_scopes: vec![SCOPE_READ.to_string()],
                scopes_supported: vec![SCOPE_READ.to_string(), SCOPE_WRITE.to_string()],
                resource_name: Some("crucible-git".to_string()),
                cluster: None,
            },
            &JwksTls {
                ca_bundle: args.jwks_ca,
                insecure_http: false,
            },
        )
        .context("building the resource server")?,
    );
    if args.token_file.is_none() {
        tracing::warn!("no GitHub App token file; every call is refused");
    }
    let gh = Arc::new(github::GitHub::new(args.token_file, args.api_url)?);

    let sandbox = Arc::new(Sandbox {
        server: server.clone(),
        run_id: args.run_id.clone(),
    });
    let sandbox_layer = axum::middleware::from_fn_with_state(sandbox, authenticate);
    let allow = Arc::new(allow::Allow::new(args.bounds_file));
    let relay = Arc::new(relay::Relay {
        gh: gh.clone(),
        allow: allow.clone(),
        policy: policy::State::default(),
    });
    let sandbox_app = proxy::router(relay.clone())
        .layer(sandbox_layer.clone())
        .merge(mcp::router(
            gh.clone(),
            allow.clone(),
            &args.allowed_hosts,
            sandbox_layer,
            Some(server),
        ));

    let mitm = mitm::Mitm::new(relay).context("starting the GitHub TLS listener")?;
    mitm.write(&args.ca_dir)?;
    let tls_listener = tokio::net::TcpListener::bind(&args.tls_bind)
        .await
        .with_context(|| format!("binding {}", args.tls_bind))?;
    let sandbox_listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!(
        "crucible-git serving run {} to sandboxes on http://{} and to tasks as GitHub on {}",
        args.run_id,
        args.bind,
        args.tls_bind
    );
    tokio::try_join!(
        async {
            axum::serve(sandbox_listener, sandbox_app)
                .await
                .context("serving sandboxes")
        },
        mitm::serve(Arc::new(mitm), tls_listener),
    )
    .map(|_| ())
    .context("serving crucible-git")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Cmd::Serve(args) => serve(args).await,
    }
}
