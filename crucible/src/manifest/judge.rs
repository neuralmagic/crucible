use crate::command_judge::Direction;
use crate::manifest::selftest::SelftestCfg;
use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeCfg {
    pub measure_cmd: String,
    pub direction: String,
    #[serde(default = "default_objective")]
    pub objective: String,
    /// The gate self-test: negative controls the gate must tell apart before it's trusted. Optional;
    /// absent means the gate hasn't been proven to discriminate (a `crucible check` warning, not
    /// yet a hard requirement).
    #[serde(default)]
    pub selftest: Option<SelftestCfg>,
    /// Skip the iter-0 baseline measure (snapshot only). For codegen domains: no meaningful
    /// pristine baseline, and the measure path needs the sandbox that isn't up yet.
    #[serde(default)]
    pub skip_baseline: bool,
}

fn default_objective() -> String {
    "score".to_string()
}

/// Parse a `[judge].direction` string. Shared by [`Manifest`] and [`CompositeManifest`].
pub fn parse_direction(s: &str) -> Result<Direction> {
    match s {
        "lower" => Ok(Direction::Lower),
        "higher" => Ok(Direction::Higher),
        other => bail!("[judge].direction must be \"lower\" or \"higher\", got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_baseline_parses_and_defaults_off() {
        let j: JudgeCfg =
            toml::from_str("measure_cmd = \"gate\"\ndirection = \"higher\"").expect("parses");
        assert!(!j.skip_baseline);
        let j: JudgeCfg =
            toml::from_str("measure_cmd = \"gate\"\ndirection = \"higher\"\nskip_baseline = true")
                .expect("parses");
        assert!(j.skip_baseline);
    }
}
