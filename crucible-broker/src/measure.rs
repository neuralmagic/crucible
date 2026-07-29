//! Broker-side measurement. The sandboxed agent has no cluster-network position, so the loop pod
//! runs the manifest's `[judge].measure_cmd` (forwarded verbatim as `BROKER_MEASURE_CMD`) and the
//! agent gets back the SAME number the judge will gate on: one source of truth, one instrument.

use serde_json::json;
use std::process::Command;

/// Run the judge's `measure_cmd` engine-side and return its JSON. On success the measure_cmd's own
/// JSON record is handed back verbatim (the agent reads `valid` / `score` / the detail fields, the
/// same shape the loop's RESULTS table records); failures come back as `{"status":"error",...}`.
pub(crate) fn measure() -> String {
    let cmd = match std::env::var("BROKER_MEASURE_CMD")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(c) => c,
        None => {
            return json!({
                "status": "error",
                "error": "measurement not configured for this run (BROKER_MEASURE_CMD unset)"
            })
            .to_string();
        }
    };

    match Command::new("sh").arg("-c").arg(&cmd).output() {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                json!({"status": "error", "error": "measure produced no output"}).to_string()
            } else {
                // The judge's measure_cmd prints one JSON line; hand it back as-is.
                s
            }
        }
        Ok(out) => json!({
            "status": "error",
            "error": format!("measure failed: {}", String::from_utf8_lossy(&out.stderr).trim())
        })
        .to_string(),
        Err(e) => json!({
            "status": "error",
            "error": format!("could not run measure: {e}")
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no measure_cmd configured, the tool reports a clear error rather than running anything.
    /// (The test process doesn't set BROKER_MEASURE_CMD.)
    #[test]
    fn unconfigured_measure_is_a_clean_error() {
        // Guard against a stray env in the test runner.
        if std::env::var("BROKER_MEASURE_CMD").is_ok() {
            return;
        }
        let out = measure();
        assert!(out.contains(r#""status":"error""#), "{out}");
        assert!(out.contains("BROKER_MEASURE_CMD"), "{out}");
    }
}
