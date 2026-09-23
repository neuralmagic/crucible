//! `DATABASE_URL=embedded`: a Postgres the daemon installs and runs itself, its data under the
//! state dir.

use anyhow::{Context, Result};
use postgresql_embedded::{PostgreSQL, Settings, VersionReq};
use rand::distr::{Alphanumeric, SampleString};
use std::path::Path;
use std::time::Duration;

/// The database the controller's schema lives in.
const DATABASE: &str = "crucible";

/// Where the embedded binaries are fetched from on first use.
const RELEASES_URL: &str = "https://github.com/theseus-rs/postgresql-binaries";

/// The Postgres major version, the one CI runs.
const MAJOR: &str = "^16";

/// A running embedded Postgres. Dropping it stops the server; the data stays.
pub struct EmbeddedDb {
    _server: PostgreSQL,
}

impl EmbeddedDb {
    /// Install Postgres if this machine has not yet, initialize `state_dir/postgres` on first use,
    /// start the server on a free loopback port, and return the URL of the controller's database.
    pub async fn start(state_dir: &Path) -> Result<(Self, String)> {
        let root = std::path::absolute(state_dir.join("postgres"))
            .context("resolving the embedded Postgres directory")?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let password_file = root.join("pgpass");
        let password = match std::fs::read_to_string(&password_file) {
            Ok(saved) => saved.trim().to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Alphanumeric.sample_string(&mut rand::rng(), 32)
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", password_file.display()));
            }
        };
        let settings = Settings {
            releases_url: RELEASES_URL.to_string(),
            version: VersionReq::parse(MAJOR).context("parsing the embedded Postgres version")?,
            installation_dir: installation_dir()?,
            password_file,
            data_dir: root.join("data"),
            host: "127.0.0.1".to_string(),
            port: 0,
            username: postgresql_embedded::BOOTSTRAP_SUPERUSER.to_string(),
            password,
            temporary: false,
            timeout: Some(Duration::from_secs(60)),
            configuration: Default::default(),
            trust_installation_dir: false,
            socket_dir: None,
        };
        let mut server = PostgreSQL::new(settings);
        server
            .setup()
            .await
            .context("installing and initializing the embedded Postgres")?;
        if root.join("data").join("postmaster.pid").exists() {
            tracing::warn!("stopping an embedded Postgres a previous controller left running");
            if let Err(e) = server.stop().await {
                tracing::warn!(error = %e, "stopping the leftover embedded Postgres failed");
            }
        }
        server
            .start()
            .await
            .context("starting the embedded Postgres")?;
        if !server
            .database_exists(DATABASE)
            .await
            .context("looking up the controller database")?
        {
            server
                .create_database(DATABASE)
                .await
                .context("creating the controller database")?;
        }
        let url = server.settings().url(DATABASE);
        tracing::info!(
            data_dir = %root.join("data").display(),
            port = server.settings().port,
            "embedded Postgres running"
        );
        Ok((EmbeddedDb { _server: server }, url))
    }
}

/// The binaries cache shared by every embedded Postgres on this machine.
fn installation_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .context("HOME is unset, so there is nowhere to install Postgres")?;
    Ok(std::path::PathBuf::from(home)
        .join(".theseus")
        .join("postgresql"))
}
