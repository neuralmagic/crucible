//! Serve the debug UI over an existing controller ledger, standalone — a local viewer until
//! the daemon mounts `serve` itself (lane I). Overrides submitted from the UI are logged and
//! dropped (no daemon, no reconciler to hand them to).
//!
//!   cargo run -p crucible-controller --example serve_ui -- <state-dir> [port]

use crucible_controller::api::state::ApiState;
use crucible_controller::{ControllerCfg, Db, Override, OverrideSink, serve};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

struct LogOnly;

impl OverrideSink for LogOnly {
    fn submit(&self, ov: Override) {
        eprintln!("override received (viewer mode, dropped): {ov:?}");
    }
}

/// `ControllerCfg` derives `clap::Args`, not `Parser`; flatten it under a `Parser` wrapper so the
/// example can build one off defaults and point its `state_dir` at the ledger being viewed.
#[derive(clap::Parser)]
struct ViewerCfg {
    #[command(flatten)]
    cfg: ControllerCfg,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;

    let mut args = std::env::args().skip(1);
    let state_dir = PathBuf::from(args.next().unwrap_or_else(|| "state".into()));
    let port: u16 = args.next().as_deref().unwrap_or("8850").parse()?;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost:5432/crucible".to_string());
    let db = Db::open(&db_url).await?;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    eprintln!("debug UI on http://{addr} over {}", state_dir.display());

    let mut cfg = ViewerCfg::parse_from(["serve_ui"]).cfg;
    cfg.state_dir = state_dir.clone();

    let clusters = Arc::new(crucible_controller::runs::clusters::ClusterClients::new(
        None,
    ));
    let queue = crucible_controller::daemon::queue::WorkQueue::new();
    let reconcile_now = Arc::new(tokio::sync::Notify::new());
    let state = ApiState::new(
        db,
        Arc::new(LogOnly),
        Arc::new(queue),
        clusters,
        None,
        reconcile_now,
        Arc::new(crucible_controller::runs::contract::ContractRegistry::new(
            Arc::new(crucible_controller::runs::contract::LiveContractReader::new(None)),
        )),
        crucible_controller::authz::policy::ActivePolicy::default_set()
            .expect("the shipped default policy set loads"),
        &cfg,
    );
    serve(
        state,
        addr,
        crucible_controller::config::TurnAccounts::default(),
        cfg.session_secure_cookies,
    )
    .await
}
