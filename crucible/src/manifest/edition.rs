//! A pack's edition: the version of the pack contract its manifest is written against. An engine
//! runs each edition it supports with that edition's semantics, so a pack changes behaviour only
//! when its author raises the edition.

use std::fmt;

/// The top-level `edition` key. Absent is edition 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(try_from = "i64")]
pub enum Edition {
    /// Host commands inherit the engine's environment.
    #[default]
    One,
    /// Host commands receive the base set, the engine's own variables for them, and what their
    /// table's `env` declares (RFC-0001:C-COMMANDS).
    Two,
}

impl Edition {
    /// Every edition this engine runs, oldest first.
    pub const SUPPORTED: [Edition; 2] = [Edition::One, Edition::Two];

    /// The newest edition this engine runs.
    pub const LATEST: Edition = Edition::SUPPORTED[Edition::SUPPORTED.len() - 1];

    pub fn number(self) -> i64 {
        match self {
            Edition::One => 1,
            Edition::Two => 2,
        }
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.number())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "edition = {edition} is not an edition this engine supports; it supports {}",
    supported()
)]
pub struct UnknownEdition {
    edition: i64,
}

fn supported() -> String {
    Edition::SUPPORTED
        .iter()
        .map(Edition::to_string)
        .collect::<Vec<_>>()
        .join(" and ")
}

impl TryFrom<i64> for Edition {
    type Error = UnknownEdition;

    fn try_from(edition: i64) -> Result<Self, Self::Error> {
        Edition::SUPPORTED
            .into_iter()
            .find(|known| known.number() == edition)
            .ok_or(UnknownEdition { edition })
    }
}

/// The edition manifest `text` declares, read without parsing or validating the rest of it.
pub fn declared(text: &str) -> Result<Edition, toml::de::Error> {
    #[derive(serde::Deserialize)]
    struct Declared {
        #[serde(default)]
        edition: Edition,
    }
    toml::from_str::<Declared>(text).map(|d| d.edition)
}

/// What `crucible check` and run start say about an edition-1 pack whose host commands inherit
/// the engine's environment.
pub const INHERITED_ENV_WARNING: &str = "this pack is edition 1, so measure_cmd, the [world] \
     hooks, and workflow command and evaluate tasks inherit the engine's whole environment, \
     credentials included; set `edition = 2` at the top of the manifest and declare what they \
     read in [judge].env, [world].env, or [workflow].env";

#[cfg(test)]
mod tests {
    use crate::manifest::edition::{Edition, UnknownEdition};

    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        edition: Edition,
    }

    fn parse(text: &str) -> Result<Edition, String> {
        toml::from_str::<Doc>(text)
            .map(|doc| doc.edition)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn an_absent_edition_is_one_and_one_and_two_read_as_written() {
        assert_eq!(parse(""), Ok(Edition::One));
        assert_eq!(parse("edition = 1"), Ok(Edition::One));
        assert_eq!(parse("edition = 2"), Ok(Edition::Two));
    }

    #[test]
    fn an_unknown_edition_is_refused_naming_the_editions_this_engine_supports() {
        for (edition, written) in [(0, "edition = 0"), (3, "edition = 3"), (-2, "edition = -2")] {
            assert_eq!(Edition::try_from(edition), Err(UnknownEdition { edition }));
            let error = parse(written).unwrap_err();
            assert!(
                error.contains(&format!(
                    "edition = {edition} is not an edition this engine supports; it supports 1 \
                     and 2"
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn an_edition_that_is_not_an_integer_is_refused() {
        assert!(parse("edition = \"2\"").is_err());
        assert!(parse("edition = 2.0").is_err());
    }

    #[test]
    fn the_declared_edition_reads_past_tables_it_does_not_know() {
        let text = "edition = 2\n[agent]\ngoal = \"g\"\n[workflow]\nunknown = 1\n";
        assert_eq!(crate::manifest::edition::declared(text), Ok(Edition::Two));
        assert_eq!(
            crate::manifest::edition::declared("[agent]\n"),
            Ok(Edition::One)
        );
        let error = crate::manifest::edition::declared("edition = 9\n").unwrap_err();
        assert!(
            error.to_string().contains("edition = 9 is not an edition"),
            "{error}"
        );
    }
}
