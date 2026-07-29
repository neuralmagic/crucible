//! Background approval watcher → control-bridge client (async approval path).
//!
//! When `request_trace` opens a judge-changing approval (`Resolution::PendingApproval`), the broker
//! watches it out-of-band: it polls the approval backend until a human resolves it, and on a yes it
//! sends a `rescope` command to the loop's control bridge (`crucible/src/control.rs`). The loop (parked
//! or still iterating) drains that rescope and re-baselines into the granted regime. The agent never
//! holds the forge token or the control socket; only the broker does.

use crate::approval::ApprovalState;
use crate::broker::Broker;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// How often to poll an open approval. Coarse on purpose; a human approval is minutes-to-hours.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The control-bridge address crucible injects when it spawns the broker with a control port.
const CONTROL_ADDR_ENV: &str = "BROKER_CONTROL_ADDR";

/// Watch an opened approval in the background and fire a `rescope` to the loop's control bridge when
/// it is granted. A no-op (logged) when there is no control bridge to notify; the broker still ran,
/// the agent just won't get an automatic resume.
pub(crate) fn spawn_approval_watch(broker: Arc<Broker>, handle: String, regime: String) {
    let Some(addr) = control_addr() else {
        tracing::warn!(%handle, "approval opened but {CONTROL_ADDR_ENV} is unset — no control bridge to notify");
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let broker = broker.clone();
            let h = handle.clone();
            // `poll` shells `gh api` (blocking); keep it off the async worker threads.
            match tokio::task::spawn_blocking(move || broker.approval.poll(&h)).await {
                Ok(Ok(ApprovalState::Approved)) => {
                    // CAPTURE-WAIT BOUNDARY. The grant is approved, but the actual provisioning (the GPU
                    // capture that produces the calibrated trace + `MOCK_*` knobs) must COMPLETE before
                    // the loop re-baselines, or it measures an uncalibrated regime. That capture submit
                    // + poll-until-ready belongs *here*, between approval and the rescope, and needs the
                    // Kueue/trace backend (the deferred "finish the broker" work). The loop is already
                    // parked waiting for the terminal rescope, so when the backend lands only this block
                    // changes. Until then we send rescope on approval.
                    tracing::info!(%handle, %regime, %addr, "approval granted — sending rescope");
                    let cmd = serde_json::json!({ "cmd": "rescope", "regime": regime });
                    if let Err(e) = send_command(&addr, &cmd) {
                        tracing::error!(%addr, "failed to send rescope: {e:#}");
                    }
                    break;
                }
                Ok(Ok(ApprovalState::Denied)) => {
                    // Deny path: tell the loop the ask was rejected. A blocked run escalates (no fallback);
                    // a continuing run just stays in the frozen regime.
                    tracing::info!(%handle, %addr, "approval denied — notifying the loop");
                    let cmd = serde_json::json!({ "cmd": "deny", "reason": "approval denied" });
                    if let Err(e) = send_command(&addr, &cmd) {
                        tracing::error!(%addr, "failed to send deny: {e:#}");
                    }
                    break;
                }
                Ok(Ok(ApprovalState::Pending)) => continue,
                Ok(Err(e)) => tracing::warn!(%handle, "approval poll error (will retry): {e:#}"),
                Err(e) => {
                    tracing::error!(%handle, "approval poll task failed: {e}");
                    break;
                }
            }
        }
    });
}

/// The control-bridge address, if crucible gave us one.
fn control_addr() -> Option<String> {
    std::env::var(CONTROL_ADDR_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

/// Send one NDJSON control command to the loop's control bridge and return once it's flushed. The
/// bridge replies with a JSON ack we don't need to read for correctness.
fn send_command(addr: &str, cmd: &serde_json::Value) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    writeln!(stream, "{cmd}")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn send_command_writes_the_exact_ndjson_line() {
        // A real listener stands in for the control bridge; assert the exact line it receives,
        // the same shape `crucible/src/control.rs` parses into a `ControlCommand`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).expect("read");
            line
        });

        let cmd = serde_json::json!({ "cmd": "rescope", "regime": "model=Qwen/Qwen3-0.6B;c=48" });
        send_command(&addr, &cmd).expect("send");

        let line = server.join().expect("join");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
        assert_eq!(v["cmd"], "rescope");
        assert_eq!(v["regime"], "model=Qwen/Qwen3-0.6B;c=48");
    }
}
