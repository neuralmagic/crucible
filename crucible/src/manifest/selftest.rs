use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("[judge.selftest].runs must be >= 1, got 0")]
pub struct ZeroSelftestRuns;

/// `[judge.selftest]`: a known-good and a known-bad config, each a command that stages it into
/// the workspace/world. Both are required once the table is present, a self-test that only
/// stages one side isn't a control. `runs` averages a noisy gate's score per control.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SelftestCfg {
    /// Stages the known-good configuration (run in the workspace, same conventions as other
    /// domain commands).
    pub good_cmd: String,
    /// Stages the known-bad configuration.
    pub bad_cmd: String,
    #[serde(default = "default_selftest_runs")]
    pub runs: u32,
}

fn default_selftest_runs() -> u32 {
    1
}

/// Shared by [`Manifest`]/[`CompositeManifest`] validation: `[judge.selftest].runs` must be at
/// least 1 (a mean over zero runs is meaningless).
pub fn validate_selftest(selftest: &Option<SelftestCfg>) -> Result<(), ZeroSelftestRuns> {
    let Some(s) = selftest else { return Ok(()) };
    if s.runs == 0 {
        return Err(ZeroSelftestRuns);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;

    fn manifest_toml_with_judge_extra(extra: &str) -> String {
        format!(
            r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [judge]
            measure_cmd = "m"
            direction = "higher"
            {extra}
        "#
        )
    }

    #[test]
    fn selftest_cfg_parses_with_both_commands() {
        let toml = manifest_toml_with_judge_extra(
            r#"
            [judge.selftest]
            good_cmd = "apply-known-good"
            bad_cmd = "apply-known-bad"
        "#,
        );
        let m: Manifest = toml::from_str(&toml).expect("parses");
        let s = m.judge.selftest.as_ref().expect("selftest present");
        assert_eq!(s.good_cmd, "apply-known-good");
        assert_eq!(s.bad_cmd, "apply-known-bad");
        assert_eq!(s.runs, 1, "runs defaults to 1");
        m.validate().expect("valid");
    }

    #[test]
    fn selftest_cfg_absent_by_default() {
        let toml = manifest_toml_with_judge_extra("");
        let m: Manifest = toml::from_str(&toml).expect("parses");
        assert!(m.judge.selftest.is_none());
    }

    #[test]
    fn selftest_cfg_requires_both_commands() {
        // Only good_cmd given: bad_cmd is required once the table is present.
        let toml = manifest_toml_with_judge_extra(
            r#"
            [judge.selftest]
            good_cmd = "apply-known-good"
        "#,
        );
        assert!(
            toml::from_str::<Manifest>(&toml).is_err(),
            "missing bad_cmd must fail to parse"
        );
    }

    #[test]
    fn selftest_cfg_runs_zero_rejected() {
        let toml = manifest_toml_with_judge_extra(
            r#"
            [judge.selftest]
            good_cmd = "good"
            bad_cmd = "bad"
            runs = 0
        "#,
        );
        let m: Manifest = toml::from_str(&toml)
            .expect("parses (runs=0 is a validate() error, not a parse error)");
        assert!(m.validate().is_err(), "runs must be >= 1");
    }

    #[test]
    fn selftest_cfg_custom_runs() {
        let toml = manifest_toml_with_judge_extra(
            r#"
            [judge.selftest]
            good_cmd = "good"
            bad_cmd = "bad"
            runs = 5
        "#,
        );
        let m: Manifest = toml::from_str(&toml).expect("parses");
        assert_eq!(m.judge.selftest.unwrap().runs, 5);
    }
}
