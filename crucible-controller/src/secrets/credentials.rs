//! The value shape of an `inference_api_key` secret: one credential, or a map of them keyed by the
//! environment variable each lands in. The map is OpenShell's provider `credentials` map, so a
//! secret registered here can be handed to its gateway as one provider record. Nothing here reads
//! Vault: a caller hands in the bytes and gets back what they mean.

use std::collections::BTreeMap;

/// The characters an environment variable name may carry: what a POSIX shell exports and what a
/// pod spec accepts.
fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    #[error("credentials are a JSON object of environment variable to value: {0}")]
    NotAnObject(String),
    #[error(
        "credentials name {name:?}, which is not an environment variable name ([A-Z_][A-Z0-9_]*)"
    )]
    BadName { name: String },
    #[error("credential {name} is empty")]
    Empty { name: String },
    #[error("credential {name} is not a string")]
    NotAString { name: String },
    #[error("credentials name no variable at all")]
    NoEntries,
    #[error("credentials name no {expected}, the entry the provider's key is read from")]
    MissingKey { expected: &'static str },
    #[error("a {protocol} provider's secret is one bare key, not a map")]
    MapForBareKey { protocol: &'static str },
}

/// What an inference secret holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// One bare key. It lands in whichever variable the provider that spends it reads.
    Single(String),
    /// Named credentials, each landing in the variable it is keyed by.
    Map(BTreeMap<String, String>),
}

impl Credentials {
    /// Read a stored value. Bytes that open with `{` are a map and have to parse as one; anything
    /// else is a single key, which is what every registration before maps existed stored.
    pub fn parse(raw: &str) -> Result<Credentials, CredentialsError> {
        let trimmed = raw.trim();
        if !trimmed.starts_with('{') {
            if trimmed.is_empty() {
                return Err(CredentialsError::NoEntries);
            }
            return Ok(Credentials::Single(trimmed.to_string()));
        }
        let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(trimmed)
            .map_err(|e| CredentialsError::NotAnObject(e.to_string()))?;
        let mut map = BTreeMap::new();
        for (name, value) in object {
            if !is_env_name(&name) {
                return Err(CredentialsError::BadName { name });
            }
            let Some(value) = value.as_str() else {
                return Err(CredentialsError::NotAString { name });
            };
            if value.trim().is_empty() {
                return Err(CredentialsError::Empty { name });
            }
            map.insert(name, value.to_string());
        }
        if map.is_empty() {
            return Err(CredentialsError::NoEntries);
        }
        Ok(Credentials::Map(map))
    }

    /// The key a provider spends, and the map's other entries. A single key is the key; a map
    /// keeps it under `entry`, the variable a client of the provider's protocol reads it from, or
    /// is refused when the protocol takes one bare key (`entry` is `None`).
    pub fn split(
        self,
        protocol: &'static str,
        entry: Option<&'static str>,
    ) -> Result<(String, BTreeMap<String, String>), CredentialsError> {
        match (self, entry) {
            (Credentials::Single(value), _) => Ok((value, BTreeMap::new())),
            (Credentials::Map(_), None) => Err(CredentialsError::MapForBareKey { protocol }),
            (Credentials::Map(mut map), Some(entry)) => {
                let key = map
                    .remove(entry)
                    .ok_or(CredentialsError::MissingKey { expected: entry })?;
                Ok((key, map))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::secrets::credentials::*;

    #[test]
    fn a_bare_key_is_the_key_whatever_the_protocol() {
        let creds = Credentials::parse("  sk-test\n").expect("parses");
        assert_eq!(creds, Credentials::Single("sk-test".to_string()));
        for entry in [Some("OPENAI_API_KEY"), None] {
            assert_eq!(
                creds.clone().split("decisions", entry).expect("a bare key"),
                ("sk-test".to_string(), BTreeMap::new())
            );
        }
    }

    #[test]
    fn a_map_gives_up_its_protocol_entry_as_the_key_and_keeps_the_rest() {
        let creds =
            Credentials::parse(r#"{"OPENAI_API_KEY": "sk-test", "OPENAI_ORG_ID": "org-1"}"#)
                .expect("parses");
        let (key, rest) = creds
            .clone()
            .split("responses", Some("OPENAI_API_KEY"))
            .expect("carries the key");
        assert_eq!(key, "sk-test");
        assert_eq!(
            rest,
            BTreeMap::from([("OPENAI_ORG_ID".to_string(), "org-1".to_string())])
        );
        let err = creds
            .clone()
            .split("messages", Some("ANTHROPIC_API_KEY"))
            .expect_err("the wrong protocol's key is missing");
        assert!(matches!(
            err,
            CredentialsError::MissingKey {
                expected: "ANTHROPIC_API_KEY"
            }
        ));
        let err = creds
            .split("system_one", None)
            .expect_err("a bare-key protocol refuses a map");
        assert!(err.to_string().contains("system_one"), "{err}");
    }

    #[test]
    fn a_map_is_refused_when_it_is_not_a_clean_env_map() {
        for (raw, why) in [
            (r#"{"openai_api_key": "x"}"#, "lowercase"),
            (r#"{"OPENAI_API_KEY": ""}"#, "empty value"),
            (r#"{"OPENAI_API_KEY": 5}"#, "non-string"),
            (r#"{}"#, "no entries"),
            (r#"{"OPENAI_API_KEY": "x""#, "not JSON"),
            (r#"{"BAD-NAME": "x"}"#, "dash"),
            ("", "empty"),
        ] {
            assert!(Credentials::parse(raw).is_err(), "{why}: {raw:?}");
        }
    }
}
