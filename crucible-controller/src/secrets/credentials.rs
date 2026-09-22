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
    #[error("credentials name no {expected}, which the provider's harness reads its key from")]
    MissingKey { expected: &'static str },
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

    /// The variables this secret lands in when a provider whose harness reads `key_env` spends it.
    /// A single key takes that name; a map has to carry it, or the harness would start without a
    /// key while other variables were set.
    pub fn for_env(
        self,
        key_env: &'static str,
    ) -> Result<BTreeMap<String, String>, CredentialsError> {
        match self {
            Credentials::Single(value) => Ok(BTreeMap::from([(key_env.to_string(), value)])),
            Credentials::Map(map) => {
                if !map.contains_key(key_env) {
                    return Err(CredentialsError::MissingKey { expected: key_env });
                }
                Ok(map)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_key_is_a_single_credential_under_the_providers_variable() {
        let creds = Credentials::parse("  sk-test\n").expect("parses");
        assert_eq!(creds, Credentials::Single("sk-test".to_string()));
        assert_eq!(
            creds.for_env("OPENAI_API_KEY").expect("named"),
            BTreeMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])
        );
    }

    #[test]
    fn a_map_keeps_every_variable_and_must_carry_the_harness_key() {
        let creds =
            Credentials::parse(r#"{"OPENAI_API_KEY": "sk-test", "OPENAI_ORG_ID": "org-1"}"#)
                .expect("parses");
        let env = creds
            .clone()
            .for_env("OPENAI_API_KEY")
            .expect("carries the key");
        assert_eq!(env.len(), 2);
        assert_eq!(env["OPENAI_ORG_ID"], "org-1");
        let err = creds
            .for_env("ANTHROPIC_API_KEY")
            .expect_err("the wrong harness's key is missing");
        assert!(matches!(
            err,
            CredentialsError::MissingKey {
                expected: "ANTHROPIC_API_KEY"
            }
        ));
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
