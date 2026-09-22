//! The capability contract shared by the image feedstock (which stamps the
//! document onto images) and the controller (which reads and matches it).

use std::collections::BTreeMap;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

/// OCI config label the document is stored under.
pub const CAPABILITIES_LABEL: &str = "io.crucible.capabilities.v1";
/// The document's own schema identifier.
pub const CAPABILITIES_SCHEMA: &str = "io.crucible.capabilities/v1";

/// The self-description an image carries: what it provides, as namespaced
/// predicates. Field order is serialization order; keep it alphabetical so the
/// stamped label stays byte-stable across writers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CapabilityDoc {
    pub features: Vec<String>,
    pub image: String,
    pub predicates: BTreeMap<String, String>,
    pub schema: String,
}

/// One unmet requirement: what was asked, and what the document held.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Unsatisfied {
    pub predicate: String,
    pub required: String,
    /// None when the document does not declare the predicate at all, which is
    /// treated as incompatible rather than assumed present.
    pub found: Option<String>,
}

/// Evaluate a requirements map against a document. Empty result = compatible.
pub fn unsatisfied(doc: &CapabilityDoc, requires: &BTreeMap<String, String>) -> Vec<Unsatisfied> {
    let mut out = Vec::new();
    for (predicate, required) in requires {
        let found = doc.predicates.get(predicate);
        let ok = match found {
            None => false,
            Some(value) => matches(value, required),
        };
        if !ok {
            out.push(Unsatisfied {
                predicate: predicate.clone(),
                required: required.clone(),
                found: found.cloned(),
            });
        }
    }
    out
}

/// Does a document's value satisfy one requirement?
///
/// When the requirement parses as a semver range and the value as a (padded)
/// version, semver semantics apply: ">=2.1", "^0.27", "=0.28.0", "22". Anything
/// non-versionish on either side falls back to exact string equality, so
/// word-valued predicates like `toolchain.cc = "gcc"` never range-match.
pub fn matches(value: &str, required: &str) -> bool {
    if let (Some(version), Ok(req)) = (pad_version(value), VersionReq::parse(required)) {
        return req.matches(&version);
    }
    value == required
}

/// Parse a version, padding missing components: "22" -> 22.0.0, "0.27" -> 0.27.0.
fn pad_version(value: &str) -> Option<Version> {
    let mut parts = value.split('.');
    let mut nums = [0u64; 3];
    for slot in &mut nums {
        match parts.next() {
            None => break,
            Some(p) => *slot = p.parse().ok()?,
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Version::new(nums[0], nums[1], nums[2]))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{CapabilityDoc, matches, unsatisfied};

    fn doc(predicates: &[(&str, &str)]) -> CapabilityDoc {
        CapabilityDoc {
            features: vec!["base".into()],
            image: "img".into(),
            predicates: predicates
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            schema: crate::CAPABILITIES_SCHEMA.into(),
        }
    }

    #[test]
    fn semver_ranges_apply_to_versionish_values() {
        assert!(matches("2.1.251", ">=2.1"));
        assert!(!matches("2.0.14", ">=2.1"));
        assert!(matches("0.27.1", "^0.27"));
        assert!(!matches("0.28.0", "^0.27"));
        assert!(matches("0.28.0", "=0.28.0"));
        // Padded partial values.
        assert!(matches("22", ">=22"));
        assert!(matches("1.25", ">=1.24.5"));
        assert!(!matches("1.25", ">=1.25.1"));
    }

    #[test]
    fn word_values_fall_back_to_equality() {
        assert!(matches("gcc", "gcc"));
        assert!(!matches("gcc", "clang"));
        // A range against a word never matches.
        assert!(!matches("gcc", ">=1"));
    }

    #[test]
    fn unknown_required_predicate_is_incompatible() {
        let d = doc(&[("toolchain.go", "1.25.11")]);
        let mut requires = BTreeMap::new();
        requires.insert("toolchain.cuda".to_string(), ">=12".to_string());
        requires.insert("toolchain.go".to_string(), ">=1.25".to_string());
        let missing = unsatisfied(&d, &requires);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].predicate, "toolchain.cuda");
        assert_eq!(missing[0].found, None);
    }

    #[test]
    fn satisfied_requirements_report_empty() {
        let d = doc(&[("agent.claude-code", "2.1"), ("domain.vllm-dev", "0.27.1")]);
        let mut requires = BTreeMap::new();
        requires.insert("agent.claude-code".to_string(), ">=2.1".to_string());
        requires.insert("domain.vllm-dev".to_string(), "^0.27".to_string());
        assert!(unsatisfied(&d, &requires).is_empty());
    }

    #[test]
    fn doc_round_trips_the_stamped_label_shape() {
        let d = doc(&[("vcs.git", "2")]);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.starts_with("{\"features\""), "{json}");
        let back: CapabilityDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }
}
