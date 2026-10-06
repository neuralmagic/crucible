//! The autopilot binary's listeners end to end, against a real Postgres: a port it cannot bind
//! fails startup with a non-zero exit, and SIGTERM stops it cleanly with its ports released.

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

const STARTUP: Duration = Duration::from_secs(120);

async fn ledger(name: &str) -> Result<String> {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at the test server");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#
    )))
    .execute(&admin)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .execute(&admin)
        .await?;
    crucible_controller::sibling_db_url(&base, name)
}

fn free_addr() -> Result<SocketAddr> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?)
}

/// The autopilot on `db`, with its http and webhook surfaces at `api` and `hooks`, its state under
/// `dir`, its log in `dir/controller.log`, and no cluster to reach.
fn autopilot(
    db: &str,
    api: SocketAddr,
    hooks: SocketAddr,
    dir: &Path,
) -> Result<tokio::process::Child> {
    let log = std::fs::File::create(dir.join("controller.log"))?;
    tokio::process::Command::new(env!("CARGO_BIN_EXE_crucible-controller"))
        .arg("autopilot")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("KUBECONFIG", dir.join("no-kubeconfig"))
        .env("DATABASE_URL", db)
        .env("CONTROLLER_API_ADDR", api.to_string())
        .env("CONTROLLER_HOOKS_ADDR", hooks.to_string())
        .env("CONTROLLER_STATE_DIR", dir.join("state"))
        .env("CONTROLLER_SCRATCH_DIR", dir.join("scratch"))
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .context("spawning the autopilot")
}

fn log_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("controller.log")).unwrap_or_default()
}

async fn exit_of(child: &mut tokio::process::Child, within: Duration) -> Result<ExitStatus> {
    tokio::time::timeout(within, child.wait())
        .await
        .context("the autopilot is still running")?
        .context("waiting on the autopilot")
}

#[tokio::test]
async fn a_taken_webhook_port_fails_startup_with_a_nonzero_exit() -> Result<()> {
    let db = ledger("surface_hooks_taken").await?;
    let dir = tempfile::tempdir()?;
    let taken = TcpListener::bind("127.0.0.1:0")?;
    let hooks = taken.local_addr()?;
    let mut child = autopilot(&db, free_addr()?, hooks, dir.path())?;
    let status = exit_of(&mut child, STARTUP).await?;
    let log = log_of(dir.path());
    assert!(!status.success(), "exited {status}:\n{log}");
    assert!(
        log.contains(&format!("binding the webhook delivery surface to {hooks}")),
        "the exit names the port it could not bind:\n{log}"
    );
    Ok(())
}

#[tokio::test]
async fn a_taken_api_port_fails_startup_with_a_nonzero_exit() -> Result<()> {
    let db = ledger("surface_api_taken").await?;
    let dir = tempfile::tempdir()?;
    let taken = TcpListener::bind("127.0.0.1:0")?;
    let api = taken.local_addr()?;
    let mut child = autopilot(&db, api, free_addr()?, dir.path())?;
    let status = exit_of(&mut child, STARTUP).await?;
    let log = log_of(dir.path());
    assert!(!status.success(), "exited {status}:\n{log}");
    assert!(
        log.contains(&format!("binding the controller http surface to {api}")),
        "the exit names the port it could not bind:\n{log}"
    );
    Ok(())
}

/// Kubernetes and the kind harness both stop the controller with SIGTERM; it shuts down rather
/// than dying on the signal, and both listeners are free to bind again once it has exited.
#[tokio::test]
async fn sigterm_stops_the_autopilot_cleanly_and_frees_its_ports() -> Result<()> {
    let db = ledger("surface_sigterm").await?;
    let dir = tempfile::tempdir()?;
    let (api, hooks) = (free_addr()?, free_addr()?);
    let mut child = autopilot(&db, api, hooks, dir.path())?;
    let http = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + STARTUP;
    loop {
        if let Some(status) = child.try_wait()? {
            panic!(
                "the autopilot exited {status} before serving:\n{}",
                log_of(dir.path())
            );
        }
        let healthy = http
            .get(format!("http://{api}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if healthy {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the autopilot never answered /healthz:\n{}",
            log_of(dir.path())
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let pid = child.id().context("the autopilot has a pid")?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(pid)?),
        nix::sys::signal::Signal::SIGTERM,
    )?;
    let status = exit_of(&mut child, Duration::from_secs(30)).await?;
    assert!(
        status.success(),
        "SIGTERM is a clean stop, not a death by signal: {status}\n{}",
        log_of(dir.path())
    );
    TcpListener::bind(api).context("the http port is free after the exit")?;
    TcpListener::bind(hooks).context("the webhook port is free after the exit")?;
    Ok(())
}
