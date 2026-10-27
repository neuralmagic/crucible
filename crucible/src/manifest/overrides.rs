use std::collections::BTreeSet;

use serde::Deserialize;

use crate::manifest::launch::{Setting, is_model_name};

/// `[agent.overrides]`: the agent settings a launch may set on the workflow's agent tasks.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOverridesCfg {
    /// Settings a launch may set. Empty: none.
    #[serde(default)]
    pub allow: BTreeSet<Setting>,
    /// Models a launch may name. Empty: any model name.
    #[serde(default)]
    pub models: Vec<String>,
}

impl AgentOverridesCfg {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.models.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OverridesError {
    #[error("[agent.overrides].models lists models, but allow does not list \"model\"")]
    ModelsWithoutModel,
    #[error("[agent.overrides].models entry {got:?} is not a model name")]
    BadModel { got: String },
}

pub fn validate(cfg: &AgentOverridesCfg) -> Result<(), OverridesError> {
    if !cfg.models.is_empty() && !cfg.allow.contains(&Setting::Model) {
        return Err(OverridesError::ModelsWithoutModel);
    }
    if let Some(bad) = cfg.models.iter().find(|m| !is_model_name(m)) {
        return Err(OverridesError::BadModel { got: bad.clone() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::manifest::launch::Setting;
    use crate::manifest::overrides::{AgentOverridesCfg, OverridesError, validate};

    fn parse(toml: &str) -> AgentOverridesCfg {
        toml::from_str(toml).expect("parses")
    }

    #[test]
    fn an_absent_table_allows_nothing() {
        let cfg = AgentOverridesCfg::default();
        assert!(cfg.is_empty());
        validate(&cfg).expect("valid");
    }

    #[test]
    fn allow_takes_only_known_settings() {
        let cfg = parse("allow = [\"model\", \"effort\", \"harness\"]");
        assert_eq!(
            cfg.allow.into_iter().collect::<Vec<_>>(),
            [Setting::Harness, Setting::Model, Setting::Effort]
        );
        for refused in [
            "allow = [\"sandbox\"]",
            "allow = [\"gpus\"]",
            "allow = [\"Model\"]",
            "allow = \"model\"",
            "allow = [\"model\"]\nextra = 1",
        ] {
            assert!(
                toml::from_str::<AgentOverridesCfg>(refused).is_err(),
                "{refused:?} parsed"
            );
        }
    }

    #[test]
    fn models_need_model_allowed_and_must_be_model_names() {
        assert!(matches!(
            validate(&parse(
                "allow = [\"effort\"]\nmodels = [\"claude-opus-5-5\"]"
            )),
            Err(OverridesError::ModelsWithoutModel)
        ));
        assert!(matches!(
            validate(&parse("allow = [\"model\"]\nmodels = [\"--help\"]")),
            Err(OverridesError::BadModel { got }) if got == "--help"
        ));
        validate(&parse(
            "allow = [\"model\"]\nmodels = [\"claude-opus-5-5\", \"anthropic/claude-sonnet-5-5\", \"claude-opus-4-6[1m]\"]",
        ))
        .expect("valid");
    }
}
