use serde::{Deserialize, Serialize};

/// `[dispatch]`: where the controller sends every launch of this pack. The engine never reads it;
/// the controller resolves it against the clusters it can reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchCfg {
    /// The one shared cluster every launch of this pack dispatches to.
    #[serde(default)]
    pub target: Option<ClusterName>,
}

/// A shared cluster's name as a pack may pin it: what the controller's clusters directory names
/// an entry, or `hub`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ClusterName(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClusterNameError {
    #[error("[dispatch] target is empty; name a cluster or drop the key")]
    Empty,
    #[error(
        "[dispatch] target {0:?} is not a cluster name: use letters, digits, `.`, `-` and `_` \
         only (a personal target cannot be pinned by a pack)"
    )]
    Invalid(String),
}

impl TryFrom<String> for ClusterName {
    type Error = ClusterNameError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let name = raw.trim();
        if name.is_empty() {
            return Err(ClusterNameError::Empty);
        }
        let valid = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !name.starts_with('.');
        match valid {
            true => Ok(ClusterName(name.to_string())),
            false => Err(ClusterNameError::Invalid(name.to_string())),
        }
    }
}

impl ClusterName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClusterName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;
    use crate::manifest::dispatch::*;

    fn parse(extra: &str) -> anyhow::Result<Manifest> {
        Ok(toml::from_str(&format!(
            r#"
            [agent]
            backend = "command"
            agent_cmd = "a"
            goal = "g"
            [workflow]
            type = "playbook"
            file = "workflow.star"
            {extra}
            "#
        ))?)
    }

    #[test]
    fn a_manifest_without_a_dispatch_table_pins_nothing() {
        let m = parse("").expect("parses");
        assert_eq!(m.dispatch, DispatchCfg::default());
        assert_eq!(m.dispatch.target, None);
    }

    #[test]
    fn a_dispatch_target_parses_trimmed() {
        let m = parse("[dispatch]\ntarget = \" cluster-a \"").expect("parses");
        assert_eq!(
            m.dispatch.target.as_ref().map(ClusterName::as_str),
            Some("cluster-a")
        );
    }

    #[test]
    fn an_empty_dispatch_table_pins_nothing() {
        let m = parse("[dispatch]").expect("parses");
        assert_eq!(m.dispatch.target, None);
    }

    #[test]
    fn a_blank_target_is_refused_rather_than_read_as_unpinned() {
        let err = parse("[dispatch]\ntarget = \"  \"")
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("[dispatch] target is empty"), "{err}");
    }

    #[test]
    fn a_personal_target_cannot_be_pinned() {
        let err = parse("[dispatch]\ntarget = \"personal:0199c0de\"")
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("\"personal:0199c0de\""), "{err}");
        assert!(err.contains("personal target cannot be pinned"), "{err}");
    }

    #[test]
    fn names_that_could_escape_the_clusters_directory_are_refused() {
        for bad in ["../hub", "a/b", ".hidden", "with space", "café"] {
            assert_eq!(
                ClusterName::try_from(bad.to_string()),
                Err(ClusterNameError::Invalid(bad.to_string())),
                "{bad}"
            );
        }
        for good in ["hub", "cluster-a", "us_east.2"] {
            assert_eq!(
                ClusterName::try_from(good.to_string()).map(|n| n.to_string()),
                Ok(good.to_string())
            );
        }
    }

    #[test]
    fn an_unknown_dispatch_key_is_a_typo_not_a_silent_default() {
        let err = parse("[dispatch]\ncluster = \"cluster-a\"")
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("unknown field `cluster`"), "{err}");
    }
}
