//! The run's `crucible-git`, which the controller runs as a sidecar of the loop pod when it granted
//! the run the `git` server. The loop never holds the run's GitHub App token: it waits for the
//! sidecar to listen, and hands it the run's output bounds through the directory they share.
//! Its grant, egress and `.mcp.json` entry are any agent server's
//! ([`crate::openshell::agent_servers`]).

use anyhow::{Context, Result};
use crucible_contract::git::{BOUNDS_FILE, Bounds, LOCAL_BIND, SHARED_DIR};
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[error("crucible-git did not start listening on {probe} within {seconds}s")]
struct BootTimeout {
    probe: String,
    seconds: u64,
}

/// The sidecar on `port` for sandboxes and on loopback for tasks.
pub struct Git {
    pub port: u16,
}

impl Git {
    fn listening(&self) -> bool {
        crate::control::broker::port_open(&format!("127.0.0.1:{}", self.port))
            && crate::control::broker::port_open(LOCAL_BIND)
    }

    /// Wait until both of the sidecar's listeners accept connections.
    pub fn wait_ready(&self) -> Result<()> {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if self.listening() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(BootTimeout {
            probe: format!("127.0.0.1:{} and {LOCAL_BIND}", self.port),
            seconds: BOOT_TIMEOUT.as_secs(),
        }
        .into())
    }
}

/// Hand the sidecar the run's resolved output bounds. A pod without the shared directory runs no
/// sidecar, so there is nothing to write.
pub fn write_bounds(bounds: &crate::outputs::RunBounds) -> Result<()> {
    if !std::path::Path::new(SHARED_DIR).is_dir() {
        return Ok(());
    }
    let doc = Bounds {
        outputs: bounds.resolved().clone(),
        params: bounds.params().clone(),
    };
    let json = serde_json::to_vec(&doc).context("serializing the run's bounds for crucible-git")?;
    std::fs::write(BOUNDS_FILE, json).with_context(|| format!("writing {BOUNDS_FILE}"))
}
