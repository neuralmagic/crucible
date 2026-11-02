//! Named variants of one manifest. A `[variant.<name>]` table holds top-level manifest tables;
//! selecting the variant replaces each base entry of the same name whole, with no deeper merge.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The top-level table that declares variants.
pub const VARIANT_TABLE: &str = "variant";

const MAX_NAME_LEN: usize = 64;

/// A variant's name: 1-64 ASCII letters, digits, `-` or `_`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VariantName(String);

impl VariantName {
    pub fn parse(name: &str) -> Result<Self, VariantError> {
        let valid = !name.is_empty()
            && name.len() <= MAX_NAME_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(VariantError::BadName {
                name: name.to_owned(),
            });
        }
        Ok(VariantName(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VariantName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for VariantName {
    type Err = VariantError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        VariantName::parse(name)
    }
}

impl TryFrom<String> for VariantName {
    type Error = VariantError;

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
pub enum VariantError {
    #[error("variant name {name:?} must be 1-64 ASCII letters, digits, '-' or '_'")]
    BadName { name: String },
    #[error("[variant] must be a table holding one table per variant")]
    NotATable,
    #[error("[variant.{name}] must be a table of manifest tables")]
    VariantNotATable { name: String },
    #[error("[variant.{name}] declares its own [variant]; variants do not nest")]
    Nested { name: String },
    #[error(
        "the manifest declares no variant {name:?} (it declares {}){}",
        if declared.is_empty() { "none".to_owned() } else { declared.join(", ") },
        crate::plan::diag::hint(suggestion.as_deref())
    )]
    Unknown {
        name: String,
        declared: Vec<String>,
        suggestion: Option<String>,
    },
}

/// A manifest document split into its base and its declared variants.
#[derive(Debug, Clone)]
pub struct Variants {
    base: toml::Table,
    variants: BTreeMap<VariantName, toml::Table>,
}

impl Variants {
    /// Split a parsed manifest document. The base is the document without its `[variant]` table.
    pub fn split(mut document: toml::Table) -> Result<Self, VariantError> {
        let mut variants = BTreeMap::new();
        if let Some(declared) = document.remove(VARIANT_TABLE) {
            let toml::Value::Table(declared) = declared else {
                return Err(VariantError::NotATable);
            };
            for (name, table) in declared {
                let name = VariantName::parse(&name)?;
                let toml::Value::Table(table) = table else {
                    return Err(VariantError::VariantNotATable {
                        name: name.to_string(),
                    });
                };
                if table.contains_key(VARIANT_TABLE) {
                    return Err(VariantError::Nested {
                        name: name.to_string(),
                    });
                }
                variants.insert(name, table);
            }
        }
        Ok(Variants {
            base: document,
            variants,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &VariantName> {
        self.variants.keys()
    }

    /// The document a run reads: the base, or the base with each of the variant's top-level
    /// entries in place of the base entry of the same name.
    pub fn resolve(&self, variant: Option<&VariantName>) -> Result<toml::Table, VariantError> {
        let mut document = self.base.clone();
        let Some(name) = variant else {
            return Ok(document);
        };
        let Some(overrides) = self.variants.get(name) else {
            let declared: Vec<String> = self.names().map(ToString::to_string).collect();
            return Err(VariantError::Unknown {
                name: name.to_string(),
                suggestion: crate::plan::diag::suggest(
                    name.as_str(),
                    declared.iter().map(String::as_str),
                )
                .map(str::to_owned),
                declared,
            });
        };
        for (key, value) in overrides {
            document.insert(key.clone(), value.clone());
        }
        Ok(document)
    }
}

/// The variants a manifest declares, in name order.
pub fn declared(text: &str) -> anyhow::Result<Vec<VariantName>> {
    let document: toml::Table = toml::from_str(text)?;
    Ok(Variants::split(document)?.names().cloned().collect())
}

/// The manifest text a run of `variant` reads, for a reader that parses its own slice of the
/// manifest.
pub fn effective_text(text: &str, variant: Option<&VariantName>) -> anyhow::Result<String> {
    let document: toml::Table = toml::from_str(text)?;
    let resolved = Variants::split(document)?.resolve(variant)?;
    Ok(toml::to_string(&resolved)?)
}

#[cfg(test)]
mod tests {
    use crate::manifest::variant::*;

    const MANIFEST: &str = r#"
[agent]
backend = "openshell"
model = "base-model"

[[secret]]
name = "token"

[variant.offline.agent]
backend = "command"
agent_cmd = "./stand-in.sh"

[[variant.offline.secret]]
name = "fixture"

[variant.e2e.workspace]
dir = "e2e"
"#;

    fn variants() -> Variants {
        Variants::split(toml::from_str(MANIFEST).unwrap()).unwrap()
    }

    fn name(s: &str) -> VariantName {
        VariantName::parse(s).unwrap()
    }

    #[test]
    fn the_base_is_the_document_without_its_variants() {
        let base = variants().resolve(None).unwrap();
        assert!(!base.contains_key(VARIANT_TABLE));
        assert_eq!(base["agent"]["backend"].as_str(), Some("openshell"));
        assert_eq!(
            variants()
                .names()
                .map(VariantName::as_str)
                .collect::<Vec<_>>(),
            ["e2e", "offline"]
        );
    }

    #[test]
    fn a_variant_table_replaces_the_base_table_whole() {
        let offline = variants().resolve(Some(&name("offline"))).unwrap();
        let agent = offline["agent"].as_table().unwrap();
        assert_eq!(agent["backend"].as_str(), Some("command"));
        assert_eq!(agent["agent_cmd"].as_str(), Some("./stand-in.sh"));
        assert!(
            !agent.contains_key("model"),
            "no deep merge: the base [agent].model does not survive"
        );
        let secrets = offline["secret"].as_array().unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0]["name"].as_str(), Some("fixture"));
        assert!(!offline.contains_key("workspace"));
        assert!(!offline.contains_key(VARIANT_TABLE));

        let e2e = variants().resolve(Some(&name("e2e"))).unwrap();
        assert_eq!(e2e["agent"]["model"].as_str(), Some("base-model"));
        assert_eq!(e2e["workspace"]["dir"].as_str(), Some("e2e"));
    }

    #[test]
    fn an_unknown_variant_names_the_declared_ones() {
        let error = variants().resolve(Some(&name("ofline"))).unwrap_err();
        assert_eq!(
            error,
            VariantError::Unknown {
                name: "ofline".into(),
                declared: vec!["e2e".into(), "offline".into()],
                suggestion: Some("offline".into()),
            }
        );
        let message = error.to_string();
        assert!(message.contains("did you mean \"offline\"?"), "{message}");
        assert!(message.contains("e2e, offline"), "{message}");

        let none = Variants::split(toml::from_str("[agent]\n").unwrap())
            .unwrap()
            .resolve(Some(&name("offline")))
            .unwrap_err();
        assert!(none.to_string().contains("(it declares none)"), "{none}");
    }

    #[test]
    fn a_malformed_variant_table_is_refused() {
        let split = |text: &str| Variants::split(toml::from_str(text).unwrap()).unwrap_err();
        assert_eq!(split("variant = 3\n"), VariantError::NotATable);
        assert_eq!(
            split("[variant]\noffline = 3\n"),
            VariantError::VariantNotATable {
                name: "offline".into()
            }
        );
        assert_eq!(
            split("[variant.offline.variant.deeper.agent]\nbackend = \"command\"\n"),
            VariantError::Nested {
                name: "offline".into()
            }
        );
        assert_eq!(
            split("[variant.\"off line\".agent]\nbackend = \"command\"\n"),
            VariantError::BadName {
                name: "off line".into()
            }
        );
    }

    #[test]
    fn a_variant_name_is_a_short_identifier() {
        for good in ["offline", "e2e", "live_eu-1", "A"] {
            assert_eq!(name(good).as_str(), good);
        }
        for bad in ["", "off line", "a/b", "ü", &"x".repeat(65)] {
            assert!(VariantName::parse(bad).is_err(), "{bad:?}");
        }
        let round: VariantName = serde_json::from_str("\"offline\"").unwrap();
        assert_eq!(serde_json::to_string(&round).unwrap(), "\"offline\"");
        assert!(serde_json::from_str::<VariantName>("\"a b\"").is_err());
    }

    #[test]
    fn effective_text_is_what_a_slice_reader_parses() {
        let text = effective_text(MANIFEST, Some(&name("offline"))).unwrap();
        let reread: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(reread["agent"]["backend"].as_str(), Some("command"));
        assert!(!reread.contains_key(VARIANT_TABLE));
        let base = effective_text(MANIFEST, None).unwrap();
        assert!(!base.contains("[variant"), "{base}");
    }
}
