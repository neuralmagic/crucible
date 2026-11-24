//! The agent settings a manifest, a launch, and a task each name.

/// Which agent harness runs the turn. `claude` is the default everywhere; `codex` (OpenAI's
/// Codex CLI) is selected per-domain for harness ablations; `opencode` and `pi` are the harnesses
/// that speak either API family through a provider table.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    #[value(name = "opencode")]
    OpenCode,
    Pi,
}

impl Harness {
    /// The manifest and CLI spelling of the harness.
    pub const fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
            Harness::Pi => "pi",
        }
    }
}

/// The agent's reasoning-effort tier, passed to Claude Code as `--effort <level>`. A closed set
/// (Claude Code 2.1: low|medium|high|xhigh|max); unset means we don't pass the flag and Claude
/// Code picks its own default. Per-domain because the right tier is task-dependent (mechanical
/// remove-deprecated fixes need ~none; algorithmic issues want real reasoning), and it's a harness
/// hyperparameter worth ablating: does more thinking find better fixes, or just better reward hacks?
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    /// The exact token Claude Code's `--effort` flag expects.
    pub fn as_flag(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

/// A harness name, read the way a turn reads it.
pub fn parse_harness(raw: &str) -> Option<Harness> {
    <Harness as clap::ValueEnum>::from_str(raw, true).ok()
}

/// A reasoning effort, read the way a turn reads it.
pub fn parse_effort(raw: &str) -> Option<ReasoningEffort> {
    <ReasoningEffort as clap::ValueEnum>::from_str(raw, true).ok()
}

pub const HARNESS_NAMES: &str = "claude, codex, opencode or pi";
pub const EFFORT_NAMES: &str = "low, medium, high, xhigh or max";

/// An agent task's `harness`, read the way a turn reads it.
pub fn harness_knob<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Harness>, D::Error> {
    use serde::Deserialize;
    use serde::de::Error;
    Option::<String>::deserialize(d)?
        .map(|raw| {
            parse_harness(&raw)
                .ok_or_else(|| D::Error::custom(format!("harness {raw:?} is not {HARNESS_NAMES}")))
        })
        .transpose()
}

/// An agent task's `effort`, read the way a turn reads it.
pub fn effort_knob<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ReasoningEffort>, D::Error> {
    use serde::Deserialize;
    use serde::de::Error;
    Option::<String>::deserialize(d)?
        .map(|raw| {
            parse_effort(&raw)
                .ok_or_else(|| D::Error::custom(format!("effort {raw:?} is not {EFFORT_NAMES}")))
        })
        .transpose()
}

const MAX_VARIANT_NAME_LEN: usize = 64;

/// A variant's name: 1-64 ASCII letters, digits, `-` or `_`.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct VariantName(String);

impl VariantName {
    pub fn parse(name: &str) -> Result<Self, BadVariantName> {
        let valid = !name.is_empty()
            && name.len() <= MAX_VARIANT_NAME_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(BadVariantName {
                name: name.to_owned(),
            });
        }
        Ok(VariantName(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VariantName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for VariantName {
    type Err = BadVariantName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        VariantName::parse(name)
    }
}

impl TryFrom<String> for VariantName {
    type Error = BadVariantName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        VariantName::parse(&name)
    }
}

impl From<VariantName> for String {
    fn from(name: VariantName) -> Self {
        name.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("variant name {name:?} must be 1-64 ASCII letters, digits, '-' or '_'")]
pub struct BadVariantName {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use crate::knobs::VariantName;

    #[test]
    fn a_variant_name_is_a_short_identifier() {
        for good in ["offline", "e2e", "live_eu-1", "A"] {
            assert_eq!(VariantName::parse(good).unwrap().as_str(), good);
        }
        for bad in ["", "off line", "a/b", "ü", &"x".repeat(65)] {
            assert!(VariantName::parse(bad).is_err(), "{bad:?}");
        }
        let round: VariantName = serde_json::from_str("\"offline\"").unwrap();
        assert_eq!(serde_json::to_string(&round).unwrap(), "\"offline\"");
        assert!(serde_json::from_str::<VariantName>("\"a b\"").is_err());
    }
}
