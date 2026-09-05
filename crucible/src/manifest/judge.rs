use crate::crucible::Direction;
use crate::manifest::selftest::SelftestCfg;
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
#[error("[judge].direction must be \"lower\" or \"higher\", got {got:?}")]
pub struct UnknownDirection {
    got: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeCfg {
    pub measure_cmd: String,
    pub direction: String,
    /// Direction of the measure command's optional `tiebreak` scalar (the secondary axis a
    /// functional gate breaks primary-score ties on). Absent = inherits `direction`.
    #[serde(default)]
    pub tiebreak_direction: Option<String>,
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

/// Parse `[judge].tiebreak_direction`, `None` meaning "inherit `direction`".
pub fn parse_tiebreak_direction(cfg: &JudgeCfg) -> Result<Option<Direction>> {
    cfg.tiebreak_direction
        .as_deref()
        .map(parse_direction)
        .transpose()
        .context("in [judge].tiebreak_direction")
}

/// Parse a `[judge].direction` string. Shared by [`Manifest`] and [`CompositeManifest`].
pub fn parse_direction(s: &str) -> Result<Direction> {
    match s {
        "lower" => Ok(Direction::Lower),
        "higher" => Ok(Direction::Higher),
        other => Err(UnknownDirection {
            got: other.to_owned(),
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiebreak_direction_parses_and_defaults_absent() {
        let j: JudgeCfg =
            toml::from_str("measure_cmd = \"gate\"\ndirection = \"lower\"").expect("parses");
        assert!(j.tiebreak_direction.is_none());
        let j: JudgeCfg = toml::from_str(
            "measure_cmd = \"gate\"\ndirection = \"lower\"\ntiebreak_direction = \"higher\"",
        )
        .expect("parses");
        assert_eq!(j.tiebreak_direction.as_deref(), Some("higher"));
    }

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
