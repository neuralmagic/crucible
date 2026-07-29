//! The live capture backend: GPU headroom (the `gpu-check` signal) + a GPU-spend-gated capture submit
//! (`gen-capture-jobs.py` -> server-side apply, like `order-capture --execute`). All cluster access goes
//! through forge's typed kube client; no `kubectl` shell-out.
//!
//! All of this runs server-side on the loop pod; the sandboxed agent never reaches the cluster.

use crate::admission::{Admission, AdmissionDecision, GpuNeed, decide};
use crate::types::TraceParams;
use anyhow::{Context, Result, bail};
use std::process::Command;

/// Headroom + capture submission against the active kube context.
pub struct GpuCheck {
    /// GPUs to keep free after a capture admits (never grab the last idle ones).
    reserve: u32,
    /// Whether `submit` actually applies the capture (spends GPU). Off by default → dry run.
    execute: bool,
    /// inference-simulator-rs checkout (the capture-job generator lives here).
    sim_repo: String,
    /// Capture target the generator keys on (the pre-baked workload config).
    target: String,
}

impl GpuCheck {
    /// `BROKER_GPU_RESERVE` (default 2), `BROKER_EXECUTE_CAPTURE` (default off), `SIM_REPO`,
    /// `BROKER_CAPTURE_TARGET` (default `multiturn`).
    pub(crate) fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            reserve: std::env::var("BROKER_GPU_RESERVE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
            execute: std::env::var("BROKER_EXECUTE_CAPTURE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            sim_repo: std::env::var("SIM_REPO")
                .unwrap_or_else(|_| format!("{home}/llm-d/inference-simulator-rs")),
            target: std::env::var("BROKER_CAPTURE_TARGET").unwrap_or_else(|_| "multiturn".into()),
        }
    }
}

impl Admission for GpuCheck {
    fn check(&self, need: GpuNeed) -> Result<AdmissionDecision> {
        Ok(decide(forge::kube::free_gpus()?, need, self.reserve))
    }

    fn submit(&self, _params: &TraceParams) -> Result<String> {
        // The capture infra is keyed by a pre-baked `target` rather than arbitrary params, so we
        // submit the configured target.
        let gen_script = format!("{}/deploy/trace-capture/gen-capture-jobs.py", self.sim_repo);
        if !self.execute {
            return Ok(format!(
                "dry-run: would generate `python3 {gen_script} {}` and server-side apply it \
                 (set BROKER_EXECUTE_CAPTURE=1 to spend GPU)",
                self.target
            ));
        }
        let manifests = Command::new("python3")
            .arg(&gen_script)
            .arg(&self.target)
            .output()
            .context("gen-capture-jobs.py")?;
        if !manifests.status.success() {
            bail!(
                "gen-capture-jobs failed: {}",
                String::from_utf8_lossy(&manifests.stderr).trim()
            );
        }
        let yaml =
            String::from_utf8(manifests.stdout).context("capture manifests are not UTF-8")?;
        forge::kube::apply_yaml(&yaml).context("applying generated capture jobs")?;
        Ok(format!("submitted: capture target {}", self.target))
    }
}
