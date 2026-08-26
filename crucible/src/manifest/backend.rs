use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("agent backend must be local|openshell|command, got {got:?}")]
pub struct UnknownBackend {
    pub got: String,
}

/// Which backend an agent turn runs against.
///
/// `Local` runs the agent on this machine (the original behavior). `Openshell` runs it in an
/// OpenShell sandbox (Landlock + egress policy), what an in-pod loop uses so its turns are
/// isolated; it needs a `--sandbox-image` carrying the domain's toolbox binaries. `Command` runs a
/// fixed shell command as the "agent turn" (no LLM), a deterministic free proposer for testing the
/// engine end to end, see `examples/counter`.
///
/// One spelling serves `[agent].backend`, `--agent-backend`, and anything reading a rendered
/// manifest: [`FromStr`] is the only parser, and [`Default`] is the only default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    #[default]
    Local,
    Openshell,
    Command,
}

impl AgentBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            AgentBackend::Local => "local",
            AgentBackend::Openshell => "openshell",
            AgentBackend::Command => "command",
        }
    }
}

impl FromStr for AgentBackend {
    type Err = UnknownBackend;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(AgentBackend::Local),
            "openshell" => Ok(AgentBackend::Openshell),
            "command" => Ok(AgentBackend::Command),
            other => Err(UnknownBackend {
                got: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for AgentBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_round_trips_through_the_one_parser() {
        for backend in [
            AgentBackend::Local,
            AgentBackend::Openshell,
            AgentBackend::Command,
        ] {
            assert_eq!(AgentBackend::from_str(backend.as_str()), Ok(backend));
            assert_eq!(backend.to_string(), backend.as_str());
            assert_eq!(
                serde_json::to_value(backend).expect("serialize"),
                serde_json::Value::String(backend.as_str().to_owned())
            );
        }
    }

    #[test]
    fn an_unknown_spelling_names_what_it_got() {
        let error = AgentBackend::from_str("openshel").expect_err("unknown");
        assert_eq!(error.got, "openshel");
        assert!(error.to_string().contains("local|openshell|command"));
    }

    /// The manifest's `[agent].backend` and the type's own default are one fact.
    #[test]
    fn the_default_is_local() {
        assert_eq!(AgentBackend::default(), AgentBackend::Local);
        let from_toml: AgentBackend = toml::from_str("b = \"openshell\"\n")
            .map(|t: toml::Table| t["b"].clone())
            .and_then(serde::Deserialize::deserialize)
            .expect("deserialize");
        assert_eq!(from_toml, AgentBackend::Openshell);
    }
}
