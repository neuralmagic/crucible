use serde::Deserialize;

use crate::manifest::MeasureCfg;

/// `[preflight]`: the domain's rung ladder, run against the unmodified baseline tree before
/// iteration 1. Every command here costs zero agent dollars, so an environment fault (missing
/// dev headers, a shadowed pip install, an unwritable cache dir) becomes a refuse-to-start
/// verdict instead of a burned $10-25 iteration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightCfg {
    /// The rungs, in order. `{mode}` fans a command out over every build mode declared in
    /// `[measure.build].mutable_kwargs.mode`; `{digest}` interpolates the most recent `digest`
    /// a prior rung emitted in its JSON result.
    pub commands: Vec<String>,
    /// The rung whose `score`/`tiebreak` seed `segment.baseline_score`. Absent leaves the
    /// baseline at the direction's worst-score sentinel (today's codegen behavior).
    #[serde(default)]
    pub baseline: Option<String>,
}

/// The `{mode}` placeholder both `commands` and `baseline` expand over.
pub const MODE_PLACEHOLDER: &str = "{mode}";

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("[preflight].commands is empty (drop the section, or declare at least one rung)")]
    NoCommands,
    #[error(
        "[preflight] command `{command}` uses {MODE_PLACEHOLDER}, but \
         [measure.build].mutable_kwargs.mode declares no build modes"
    )]
    NoModes { command: String },
}

/// Cross-field check: a `{mode}` rung is only expandable when `[measure]` declares the mode list
/// it fans out over. Rejecting at load beats discovering it after the run has already spun up a pod.
pub fn validate_preflight(
    preflight: &Option<PreflightCfg>,
    measure: &Option<MeasureCfg>,
) -> Result<(), PreflightError> {
    let Some(cfg) = preflight else {
        return Ok(());
    };
    if cfg.commands.is_empty() {
        return Err(PreflightError::NoCommands);
    }
    let modes = measure
        .as_ref()
        .and_then(MeasureCfg::build_modes)
        .unwrap_or_default();
    let uses_mode = cfg
        .commands
        .iter()
        .chain(cfg.baseline.iter())
        .find(|c| c.contains(MODE_PLACEHOLDER));
    match uses_mode {
        Some(command) if modes.is_empty() => Err(PreflightError::NoModes {
            command: command.clone(),
        }),
        _ => Ok(()),
    }
}
