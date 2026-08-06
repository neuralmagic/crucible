use serde::Deserialize;

/// Why a `[search]` block is unusable. Checked at manifest load, before any wide round spends.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SearchError {
    #[error(
        "[search].approaches needs at least {wide} entries (one per wide candidate), got {got} \
         — diversity must be engineered, not random"
    )]
    TooFewApproaches { wide: u32, got: usize },
    #[error("[search].policy must be \"top-k\" (the only v1 policy), got {got:?}")]
    UnknownPolicy { got: String },
    #[error("[search].policy_k must be in 1..={wide} (wide), got {got}")]
    PolicyKOutOfRange { wide: u32, got: u32 },
}

/// Wide-round search config: how many candidates, which approaches, which tournament policy.
/// Required `approaches` when `wide > 0`, no auto-generated fallback.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SearchCfg {
    /// Fan-out breadth: N independent candidates per wide round. 0 or absent = no wide round.
    #[serde(default)]
    pub wide: u32,
    /// Distinct approach descriptions, one per candidate slot. REQUIRED when `wide > 0`: diversity
    /// must be engineered, not random.
    #[serde(default)]
    pub approaches: Vec<String>,
    /// Tournament policy name. v1 ships only `"top-k"`.
    #[serde(default = "default_search_policy")]
    pub policy: String,
    /// K for the `top-k` policy (how many wide-round winners seed a deep loop).
    #[serde(default = "default_policy_k")]
    pub policy_k: u32,
}

pub fn validate_search(search: &Option<SearchCfg>) -> Result<(), SearchError> {
    let Some(s) = search else { return Ok(()) };
    if s.wide == 0 {
        return Ok(());
    }
    if s.approaches.len() < s.wide as usize {
        return Err(SearchError::TooFewApproaches {
            wide: s.wide,
            got: s.approaches.len(),
        });
    }
    if s.policy != "top-k" {
        return Err(SearchError::UnknownPolicy {
            got: s.policy.clone(),
        });
    }
    if s.policy_k == 0 || s.policy_k > s.wide {
        return Err(SearchError::PolicyKOutOfRange {
            wide: s.wide,
            got: s.policy_k,
        });
    }
    Ok(())
}

fn default_search_policy() -> String {
    "top-k".to_string()
}
fn default_policy_k() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    #[test]
    fn search_cfg_valid() {
        let s = Some(SearchCfg {
            wide: 3,
            approaches: vec!["a".into(), "b".into(), "c".into()],
            policy: "top-k".into(),
            policy_k: 1,
        });
        assert!(validate_search(&s).is_ok());
    }

    #[test]
    fn search_cfg_zero_wide_always_valid() {
        let s = Some(SearchCfg {
            wide: 0,
            approaches: vec![],
            policy: "anything".into(),
            policy_k: 0,
        });
        assert!(validate_search(&s).is_ok());
    }

    #[test]
    fn search_cfg_none_is_valid() {
        assert!(validate_search(&None).is_ok());
    }

    #[test]
    fn search_cfg_too_few_approaches() {
        let s = Some(SearchCfg {
            wide: 3,
            approaches: vec!["a".into(), "b".into()],
            policy: "top-k".into(),
            policy_k: 1,
        });
        assert_eq!(
            validate_search(&s).unwrap_err(),
            SearchError::TooFewApproaches { wide: 3, got: 2 }
        );
    }

    #[test]
    fn search_cfg_bad_policy() {
        let s = Some(SearchCfg {
            wide: 2,
            approaches: vec!["a".into(), "b".into()],
            policy: "round-robin".into(),
            policy_k: 1,
        });
        let err = validate_search(&s).unwrap_err();
        assert_eq!(
            err,
            SearchError::UnknownPolicy {
                got: "round-robin".to_owned()
            }
        );
    }

    #[test]
    fn search_cfg_k_zero() {
        let s = Some(SearchCfg {
            wide: 2,
            approaches: vec!["a".into(), "b".into()],
            policy: "top-k".into(),
            policy_k: 0,
        });
        assert!(validate_search(&s).is_err());
    }

    #[test]
    fn search_cfg_k_exceeds_wide() {
        let s = Some(SearchCfg {
            wide: 2,
            approaches: vec!["a".into(), "b".into()],
            policy: "top-k".into(),
            policy_k: 3,
        });
        assert!(validate_search(&s).is_err());
    }

    #[test]
    fn search_cfg_parses_from_toml() {
        let toml = r#"
            [repo]
            path = "."
            [workspace]
            dir = "ws"
            [agent]
            backend = "command"
            agent_cmd = "true"
            [judge]
            measure_cmd = "echo 42"
            direction = "lower"

            [search]
            wide = 3
            approaches = ["cache optimization", "algorithm swap", "parallelism"]
            policy = "top-k"
            policy_k = 2
        "#;
        let dir = std::env::temp_dir().join("crucible-test-search-toml");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("crucible.toml");
        std::fs::write(&path, toml).unwrap();
        let m = Manifest::load_frozen(&path).unwrap();
        let s = m.search.unwrap();
        assert_eq!(s.wide, 3);
        assert_eq!(s.approaches.len(), 3);
        assert_eq!(s.policy, "top-k");
        assert_eq!(s.policy_k, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
